//! App updates (R-60). The desktop shell checks the signed update feed and reports what it
//! found here; the dashboard reads it from `/updates` and asks for the install, which the
//! shell carries out. Never while live, if the streamer said so.

use serde::Serialize;

use crate::frames::NoticeLevel;
use crate::{Engine, EngineError};

/// A newer version the desktop found in the update feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AvailableUpdate {
    pub version: String,
    pub notes_url: Option<String>,
}

impl Engine {
    /// The desktop shell reports the feed's answer (`None`: up to date or no feed).
    pub fn set_available_update(&self, update: Option<AvailableUpdate>) {
        *self.inner.update.lock().unwrap_or_else(|p| p.into_inner()) = update;
    }

    pub fn available_update(&self) -> Option<AvailableUpdate> {
        self.inner.update.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Live on any platform right now.
    pub fn is_live(&self) -> bool {
        self.status()["live"].as_object().is_some_and(|m| m.values().any(|v| v == true))
    }

    /// An update must wait: the streamer is live and chose "never update while live".
    pub fn update_blocked(&self) -> bool {
        self.is_live() && self.settings().never_update_while_live
    }

    /// The dashboard asks to install the update the shell found.
    pub fn request_update_install(&self) -> Result<AvailableUpdate, EngineError> {
        let Some(update) = self.available_update() else {
            return Err(EngineError::Missing {
                code: "no_update",
                message: "There's no update to install. You're on the newest version.",
                hint: "Ferrocast checks for updates every few hours on its own.",
            });
        };
        if self.update_blocked() {
            return Err(EngineError::Conflict(
                "You're live, so the update waits until the stream ends (Settings › Updates).".into(),
            ));
        }
        self.inner.install_update.notify_one();
        Ok(update)
    }

    /// The desktop shell waits here for the dashboard's install request.
    pub async fn update_install_requested(&self) {
        self.inner.install_update.notified().await;
    }

    /// The shell couldn't install: tell the streamer on the dashboard.
    pub fn update_failed(&self, why: &str) {
        self.notice(
            NoticeLevel::Warn,
            &format!("The update couldn't be installed: {why}. Ferrocast keeps running the current version."),
        );
    }
}
