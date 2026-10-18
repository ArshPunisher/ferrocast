//! Ferrocast Pro F1, the 24/7 cloud bot (docs/plan/PRO.md, milestone M7): not built yet.
//! Coming here: Twitch and Kick (M7), YouTube (M12), the desktop lease. Its routes, tables (`migrate`) and limits stay in this module.

use axum::Router;

use crate::AppState;
use crate::quota::Limit;

/// This feature's routes (none yet), merged in `crate::router`.
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
}

/// `features.cloud_bot` in `/v1/pro/status`: true only once it's built and configured here.
pub(crate) fn available(_st: &AppState) -> bool {
    false
}

/// Its per-subscription counters, listed in `/v1/pro/status` `quotas`.
pub(crate) const QUOTAS: &[Limit] = &[];
