//! Importers: StreamElements (read with the streamer's JWT, GET requests only) and
//! Streamlabs (files the streamer exports). Two steps:
//!
//! 1. **Preview**: fetch or read everything, map it to Ferrocast's shapes, and compare it
//!    with what's here. Nothing is written. The mapped data is held in memory for a while
//!    under a `preview_id`; the StreamElements token is used for this one request and then
//!    dropped, never written anywhere.
//! 2. **Apply**: a backup, then one transaction (`ferrocast_store::import`), then the
//!    engine reloads its commands, timers and points settings.

pub mod streamelements;
pub mod streamlabs;
mod text;
mod xlsx;

use std::sync::Arc;
use std::time::Duration;

use ferrocast_core::event::Platform;
use ferrocast_store::import::{self, Incoming, OnConflict, Preview, Report, Source};
use serde::Serialize;

use crate::{Engine, EngineError, now};

/// How long a preview can be applied.
pub const PREVIEW_TTL: Duration = Duration::from_secs(30 * 60);

/// The account the data came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportAccount {
    pub name: String,
    /// `None` when the other service's channel is on a platform Ferrocast doesn't support.
    pub platform: Option<Platform>,
}

/// A preview as the dashboard sees it.
#[derive(Debug, Clone, Serialize)]
pub struct ImportPreview {
    pub preview_id: String,
    pub source: Source,
    pub account: Option<ImportAccount>,
    pub expires_at: i64,
    #[serde(flatten)]
    pub preview: Preview,
}

/// The one preview that can be applied (a new preview replaces it).
#[derive(Debug)]
pub(crate) struct Held {
    id: String,
    incoming: Arc<Incoming>,
    expires_at: i64,
}

pub(crate) type Slot = Option<Held>;

/// An error with its own status, code and hint (`EngineError::Refused`).
pub(crate) fn refused(
    status: u16,
    code: &'static str,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> EngineError {
    EngineError::Refused { status, code, message: message.into(), hint: hint.into(), retry_after: None }
}

const EXPIRED: EngineError = EngineError::Missing {
    code: "preview_expired",
    message: "That preview has expired or was replaced by a newer one.",
    hint: "Run the preview again, then import.",
};

impl Engine {
    /// The StreamElements API root (tests and self-hosters point it elsewhere).
    pub fn set_streamelements_api(&self, base: &str) {
        *self.inner.streamelements_api.write().unwrap_or_else(|p| p.into_inner()) = base.trim_end_matches('/').into();
    }

    fn streamelements_api(&self) -> String {
        self.inner.streamelements_api.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Step 1 for StreamElements: reads the channel with `token` (never stored) and
    /// compares it with what's here.
    pub async fn preview_streamelements(&self, token: &str) -> Result<ImportPreview, EngineError> {
        let _one = self.inner.import_busy.try_lock().map_err(|_| busy())?;
        let token = streamelements::check_token(token)?;
        let (incoming, account) = streamelements::fetch(&self.streamelements_api(), &token).await?;
        drop(token);
        self.hold(incoming, Some(account)).await
    }

    /// Step 1 for Streamlabs: reads the exported files.
    pub async fn preview_streamlabs(&self, files: streamlabs::Files) -> Result<ImportPreview, EngineError> {
        let _one = self.inner.import_busy.try_lock().map_err(|_| busy())?;
        let incoming = tokio::task::spawn_blocking(move || streamlabs::parse(files))
            .await
            .map_err(|e| EngineError::Internal(e.to_string()))??;
        self.hold(incoming, None).await
    }

    async fn hold(&self, incoming: Incoming, account: Option<ImportAccount>) -> Result<ImportPreview, EngineError> {
        let incoming = Arc::new(incoming);
        let inc = incoming.clone();
        let preview = self.db(move |s| import::preview(s, &inc)).await?;
        let id = ferrocast_store::new_token();
        let expires_at = now().0 + PREVIEW_TTL.as_millis() as i64;
        let source = incoming.source;
        *self.inner.import.lock().unwrap_or_else(|p| p.into_inner()) =
            Some(Held { id: id.clone(), incoming, expires_at });
        Ok(ImportPreview { preview_id: id, source, account, expires_at, preview })
    }

    /// Step 2: imports what the preview showed. 404 `preview_expired` when it's gone.
    pub async fn apply_import(&self, preview_id: &str, choice: OnConflict) -> Result<Report, EngineError> {
        let _one = self.inner.import_busy.try_lock().map_err(|_| busy())?;
        let incoming = {
            let slot = self.inner.import.lock().unwrap_or_else(|p| p.into_inner());
            match slot.as_ref() {
                Some(h) if h.id == preview_id && h.expires_at > now().0 => h.incoming.clone(),
                _ => return Err(EXPIRED),
            }
        };
        let report = {
            // No settings save can land in the middle and be overwritten by the reload.
            let _config = self.config_lock().await;
            self.db(move |s| import::apply(s, &incoming, choice)).await?
        };
        {
            let mut slot = self.inner.import.lock().unwrap_or_else(|p| p.into_inner());
            if slot.as_ref().is_some_and(|h| h.id == preview_id) {
                *slot = None;
            }
        }
        self.reload().await?;
        Ok(report)
    }
}

fn busy() -> EngineError {
    EngineError::Conflict("Another import is running. Wait for it to finish, then try again.".into())
}

#[cfg(test)]
mod tests;
