//! Ferrocast Pro F5, cloud backup and sync (docs/plan/PRO.md, milestone M3): not built yet.
//! Coming here: encrypted `.ferropack` uploads in parts, kept in the `BlobStore`. Its routes, tables (`migrate`) and limits stay in this module.

use axum::Router;

use crate::AppState;
use crate::quota::Limit;

/// This feature's routes (none yet), merged in `crate::router`.
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
}

/// `features.backups` in `/v1/pro/status`: true only once it's built and configured here.
pub(crate) fn available(_st: &AppState) -> bool {
    false
}

/// Its per-subscription counters, listed in `/v1/pro/status` `quotas`.
pub(crate) const QUOTAS: &[Limit] = &[];
