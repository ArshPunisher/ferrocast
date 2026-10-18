//! Automatic backups (R-34): one a day while Ferrocast runs (and at start when the newest is
//! more than a day old), and one right before an update installs. Migrations and imports
//! take their own in the store.

use std::time::Duration;

use ferrocast_store::backup;

use crate::{Engine, EngineError};

/// How old the newest backup may get before a daily one is taken.
pub(crate) const DAILY_EVERY_MS: i64 = 24 * 3600 * 1000;
/// How often the daily task looks again. Short enough that a computer that slept through
/// the deadline catches up within the hour; the clock, not the timer, decides.
const RECHECK: Duration = Duration::from_secs(3600);

impl Engine {
    /// Takes the daily backup when the newest one is more than a day old. Returns whether
    /// one was taken.
    pub async fn daily_backup_if_due(&self) -> Result<bool, EngineError> {
        self.daily_backup_due_at(crate::now().0).await
    }

    pub(crate) async fn daily_backup_due_at(&self, now_ms: i64) -> Result<bool, EngineError> {
        let dir = self.store().dir().to_path_buf();
        let newest = self.db(move |_| backup::newest(&dir)).await?;
        if newest.is_some_and(|t| now_ms.saturating_sub(t) < DAILY_EVERY_MS) {
            return Ok(false);
        }
        self.db(|s| s.snapshot(backup::DAILY)).await?;
        Ok(true)
    }

    /// The copy taken right before an update replaces the app, so "restore previous" has
    /// this version's data.
    pub(crate) async fn backup_before_update(&self) -> Result<(), EngineError> {
        self.db(|s| s.snapshot(backup::BEFORE_UPDATE)).await.map(|_| ()).map_err(|e| {
            tracing::error!(error = %e, "no backup before the update");
            EngineError::Unavailable(format!(
                "Ferrocast couldn't back up your data before updating, so the update didn't start ({e}). \
                 Check that the disk isn't full, then press Update again."
            ))
        })
    }
}

/// The daily backup task.
pub(crate) async fn run(engine: Engine) {
    loop {
        match engine.daily_backup_if_due().await {
            Ok(true) => {
                tracing::info!("took the daily backup");
                // Pro: the cloud copy follows when the data changed (cloud_backup.rs).
                engine.cloud_backup_after_daily();
            }
            Ok(false) => {}
            Err(err) => tracing::warn!(%err, "couldn't take the daily backup; trying again in an hour"),
        }
        tokio::time::sleep(RECHECK).await;
    }
}
