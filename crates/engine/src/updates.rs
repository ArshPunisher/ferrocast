//! App updates (R-60). The desktop shell checks the signed update feed and reports here
//! what it found, how a download is going and what went wrong; the dashboard reads it from
//! `/updates` (and the live `update` frame) and asks for a check, the install and the
//! restart, which the shell carries out. Never while live, if the streamer said so, and
//! never a restart nobody asked for.
//!
//! The headless app has no updater: nothing attaches one, so `can_install` stays false and
//! every request that needs the shell answers 409 `no_updater` with where to download.

use std::time::Duration;

use serde::Serialize;

use crate::frames::{LiveFrame, NoticeLevel};
use crate::{Engine, EngineError};

/// The running build's version (the workspace version: engine and app ship together).
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Where to download by hand when the app can't update itself.
pub const DOWNLOAD_URL: &str = "https://ferrocast.clearcraftlabs.com/download";
/// How long `POST /updates/check` waits for the shell's answer before replying "still
/// checking" (the dashboard then follows the `update` frame).
pub const CHECK_WAIT: Duration = Duration::from_secs(20);

/// A newer version the desktop found in the update feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AvailableUpdate {
    pub version: String,
    /// The release page on GitHub.
    pub notes_url: Option<String>,
    /// What's new, as plain text (the feed's `notes`), when the release has any.
    pub notes: Option<String>,
    /// When it was published (RFC 3339), from the feed.
    pub published_at: Option<String>,
}

/// Why a check or an install didn't work, in words the streamer can act on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateProblem {
    /// `offline`, `feed_unavailable`, `no_build`, `bad_signature`, `download_failed`,
    /// `install_failed`, `no_permission`, `went_live`, `gone`, `update_failed`.
    pub code: String,
    pub message: String,
    pub hint: String,
}

impl UpdateProblem {
    pub fn new(code: &str, message: impl Into<String>, hint: impl Into<String>) -> Self {
        UpdateProblem { code: code.into(), message: message.into(), hint: hint.into() }
    }
}

/// Where an install is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallPhase {
    #[default]
    Idle,
    Downloading,
    /// Checking the signature and putting the new version in place.
    Installing,
    /// Installed (on Windows: downloaded and verified, the installer runs at the restart):
    /// the new version runs after a restart, which only the streamer starts.
    Ready,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct InstallState {
    pub phase: InstallPhase,
    /// The version being installed (or installed, when `ready`).
    pub version: Option<String>,
    pub downloaded: u64,
    /// Bytes in all, when the server said.
    pub total: Option<u64>,
    pub error: Option<UpdateProblem>,
}

/// Everything the dashboard shows about updates.
#[derive(Debug, Clone, Default)]
pub(crate) struct UpdateState {
    /// A desktop shell with the updater is attached (never in the headless app).
    updater: bool,
    available: Option<AvailableUpdate>,
    checking: bool,
    /// When the last check finished (ms since the epoch), whatever it found.
    checked_at: Option<i64>,
    /// The last check's problem; cleared by the next check that works.
    error: Option<UpdateProblem>,
    install: InstallState,
    /// Finished checks so far, so a waiting `POST /updates/check` sees its answer.
    checks_done: u64,
}

/// `GET /updates` and the live `update` frame (API.md §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateView {
    pub current: String,
    pub latest: Option<String>,
    /// Read-only: the installed build decides (a prerelease reads the beta feed).
    pub channel: &'static str,
    pub blocked_while_live: bool,
    pub notes_url: Option<String>,
    pub notes: Option<String>,
    pub published_at: Option<String>,
    /// This app can download and install it (the desktop app); false in the headless app.
    pub can_install: bool,
    pub checking: bool,
    pub checked_at: Option<i64>,
    pub error: Option<UpdateProblem>,
    pub install: InstallState,
}

/// The update channel follows the installed build: a prerelease version (`0.1.0-beta.10`)
/// reads the beta feed, which also carries newer stable releases, so a beta user lands on
/// stable when it ships and stays there; a stable version reads only the stable feed.
pub fn update_channel(version: &str) -> &'static str {
    if version.contains('-') { "beta" } else { "stable" }
}

fn refused(status: u16, code: &'static str, message: &str, hint: &str) -> EngineError {
    EngineError::Refused { status, code, message: message.into(), hint: hint.into(), retry_after: None }
}

fn no_updater() -> EngineError {
    refused(
        409,
        "no_updater",
        "This copy of Ferrocast can't update itself.",
        "Download the newest version from ferrocast.clearcraftlabs.com/download and install it over this one.",
    )
}

/// Whether an install may start now, and of what.
fn claimable(s: &UpdateState, blocked: bool) -> Result<AvailableUpdate, EngineError> {
    let Some(update) = s.available.clone() else {
        return Err(EngineError::Missing {
            code: "no_update",
            message: "There's no update to install. You're on the newest version.",
            hint: "Ferrocast checks for updates every few hours on its own, or press Check for updates.",
        });
    };
    if !s.updater {
        return Err(no_updater());
    }
    match s.install.phase {
        InstallPhase::Downloading | InstallPhase::Installing => {
            return Err(refused(
                409,
                "update_busy",
                "The update is already downloading.",
                "Wait for it to finish; the progress is in the sidebar.",
            ));
        }
        InstallPhase::Ready => {
            return Err(refused(
                409,
                "restart_needed",
                "The update is already downloaded and ready.",
                "Restart Ferrocast to finish (Restart to finish in the sidebar).",
            ));
        }
        InstallPhase::Idle | InstallPhase::Failed => {}
    }
    if blocked {
        return Err(refused(
            409,
            "live",
            "You're live, so the update waits until the stream ends.",
            "Update after your stream, or turn off Never update while I'm live in Settings › Updates.",
        ));
    }
    Ok(update)
}

impl Engine {
    fn with_update<R>(&self, f: impl FnOnce(&mut UpdateState) -> R) -> R {
        let r = f(&mut self.inner.update.lock().unwrap_or_else(|p| p.into_inner()));
        self.send_update_frame();
        r
    }

    fn send_update_frame(&self) {
        self.send_live(LiveFrame::new("update", &self.update_view()));
    }

    pub fn update_view(&self) -> UpdateView {
        let s = self.inner.update.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let found = s.available.as_ref();
        UpdateView {
            current: CURRENT_VERSION.into(),
            latest: found.map(|u| u.version.clone()),
            channel: update_channel(CURRENT_VERSION),
            blocked_while_live: self.update_blocked(),
            notes_url: found.and_then(|u| u.notes_url.clone()),
            notes: found.and_then(|u| u.notes.clone()),
            published_at: found.and_then(|u| u.published_at.clone()),
            can_install: s.updater,
            checking: s.checking,
            checked_at: s.checked_at,
            error: s.error,
            install: s.install,
        }
    }

    // ---- the desktop shell's side ----

    /// The desktop shell has an updater: the dashboard may offer checks and installs.
    pub fn attach_updater(&self) {
        self.with_update(|s| s.updater = true);
    }

    /// A check started (on its own timer or because the dashboard asked).
    pub fn update_check_started(&self) {
        self.with_update(|s| s.checking = true);
    }

    /// A check finished. `Ok(None)`: up to date. A failed check keeps what an earlier one
    /// found (it's still a real release) but says why this one didn't work.
    pub fn update_checked(&self, result: Result<Option<AvailableUpdate>, UpdateProblem>) {
        let done = self.with_update(|s| {
            s.checking = false;
            s.checked_at = Some(crate::now().0);
            s.checks_done += 1;
            match result {
                Ok(found) => {
                    s.error = None;
                    // A finished install stays "ready" until the restart, whatever the feed says.
                    if s.install.phase != InstallPhase::Ready {
                        s.available = found;
                    }
                }
                Err(problem) => s.error = Some(problem),
            }
            s.checks_done
        });
        // Never backwards, whatever order two finished checks report in.
        self.inner.update_checks.send_if_modified(|n| {
            let moved = done > *n;
            *n = (*n).max(done);
            moved
        });
    }

    /// The shell reports what an earlier version of this API called `set_available_update`
    /// (kept for tests and simple callers): a finished check that worked.
    pub fn set_available_update(&self, update: Option<AvailableUpdate>) {
        self.update_checked(Ok(update));
    }

    pub fn available_update(&self) -> Option<AvailableUpdate> {
        self.inner.update.lock().unwrap_or_else(|p| p.into_inner()).available.clone()
    }

    /// Download progress. The shell sends a few of these a second at most.
    pub fn update_progress(&self, version: &str, downloaded: u64, total: Option<u64>) {
        self.with_update(|s| {
            s.install = InstallState {
                phase: InstallPhase::Downloading,
                version: Some(version.into()),
                downloaded,
                total,
                error: None,
            };
        });
    }

    pub fn update_installing(&self, version: &str) {
        self.with_update(|s| {
            s.install.phase = InstallPhase::Installing;
            s.install.version = Some(version.into());
            s.install.error = None;
        });
    }

    /// Installed: it runs after a restart, which waits for the streamer (never automatic).
    pub fn update_ready(&self, version: &str) {
        self.with_update(|s| {
            s.install.phase = InstallPhase::Ready;
            s.install.version = Some(version.into());
            s.install.error = None;
        });
        self.notice(
            "update",
            NoticeLevel::Info,
            &format!("Ferrocast {version} is ready. Restart Ferrocast when it suits you to finish the update."),
        );
    }

    /// The shell couldn't download or install: say why on the dashboard.
    pub fn update_install_failed(&self, problem: UpdateProblem) {
        let line = format!("The update wasn't installed: {} {}", problem.message, problem.hint);
        self.with_update(|s| {
            s.install.phase = InstallPhase::Failed;
            s.install.error = Some(problem);
        });
        self.notice("update", NoticeLevel::Warn, &line);
    }

    /// Older name for a failed install with a plain reason.
    pub fn update_failed(&self, why: &str) {
        self.update_install_failed(UpdateProblem::new(
            "update_failed",
            format!("{why}."),
            "Ferrocast keeps running the current version. Try again from Settings › Updates.",
        ));
    }

    /// The shell waits here for the dashboard's install request.
    pub async fn update_install_requested(&self) {
        self.inner.install_update.notified().await;
    }

    /// The shell waits here for "Check for updates".
    pub async fn update_check_requested(&self) {
        self.inner.check_update.notified().await;
    }

    /// The shell waits here for "Restart to finish".
    pub async fn update_restart_requested(&self) {
        self.inner.restart_update.notified().await;
    }

    // ---- the dashboard's side ----

    /// Live on any platform right now.
    pub fn is_live(&self) -> bool {
        self.status()["live"].as_object().is_some_and(|m| m.values().any(|v| v == true))
    }

    /// An update must wait: the streamer is live and chose "never update while live".
    pub fn update_blocked(&self) -> bool {
        self.is_live() && self.settings().never_update_while_live
    }

    /// "Check for updates": asks the shell and waits up to `wait` for its answer.
    pub async fn check_for_updates(&self, wait: Duration) -> Result<UpdateView, EngineError> {
        let mut done = self.inner.update_checks.subscribe();
        let before = {
            let s = self.inner.update.lock().unwrap_or_else(|p| p.into_inner());
            if !s.updater {
                return Err(no_updater());
            }
            s.checks_done
        };
        done.mark_unchanged();
        self.inner.check_update.notify_one();
        let _ = tokio::time::timeout(wait, done.wait_for(|n| *n > before)).await;
        Ok(self.update_view())
    }

    /// The dashboard asks to install the update the shell found. A backup is taken first
    /// (R-34); without one the update doesn't start.
    pub async fn request_update_install(&self) -> Result<AvailableUpdate, EngineError> {
        let blocked = self.update_blocked();
        // Checked and claimed in one step, so two quick clicks can't start two installs.
        let (update, before) = {
            let mut s = self.inner.update.lock().unwrap_or_else(|p| p.into_inner());
            let update = claimable(&s, blocked)?;
            let next = InstallState {
                phase: InstallPhase::Downloading,
                version: Some(update.version.clone()),
                ..Default::default()
            };
            (update, std::mem::replace(&mut s.install, next))
        };
        self.send_update_frame();
        if let Err(e) = self.backup_before_update().await {
            // Back to where it was, so Update now works again once the disk has room.
            self.with_update(|s| s.install = before);
            return Err(e);
        }
        self.inner.install_update.notify_one();
        Ok(update)
    }

    /// "Restart to finish": only after an install, and never while live (R-60).
    pub fn request_update_restart(&self) -> Result<String, EngineError> {
        let (updater, install) = {
            let s = self.inner.update.lock().unwrap_or_else(|p| p.into_inner());
            (s.updater, s.install.clone())
        };
        if !updater {
            return Err(no_updater());
        }
        if install.phase != InstallPhase::Ready {
            return Err(refused(
                409,
                "not_ready",
                "There's no installed update waiting for a restart.",
                "Install the update first (Update now), then restart.",
            ));
        }
        if self.update_blocked() {
            return Err(refused(
                409,
                "live",
                "You're live, so Ferrocast won't restart until the stream ends.",
                "Restart after your stream. The update is ready and waits.",
            ));
        }
        self.inner.restart_update.notify_one();
        Ok(install.version.unwrap_or_default())
    }
}

#[cfg(test)]
mod channel_tests {
    use super::update_channel;

    #[test]
    fn the_running_version_picks_the_channel() {
        assert_eq!(update_channel("0.1.0-beta.10"), "beta");
        assert_eq!(update_channel("1.2.0-rc.1"), "beta");
        assert_eq!(update_channel("0.1.0"), "stable");
        assert_eq!(update_channel("1.0.0"), "stable", "a beta user who updated to stable stays on stable");
    }

    #[test]
    fn old_saved_settings_with_a_channel_still_load() {
        let old = serde_json::json!({ "theme": "dark", "update_channel": "beta" });
        let s: crate::EngineSettings = serde_json::from_value(old).unwrap();
        assert_eq!(s.theme, "dark");
        assert!(serde_json::to_value(&s).unwrap().get("update_channel").is_none());
    }
}
