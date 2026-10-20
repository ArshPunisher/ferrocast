//! The Control Center's account tabs (`/admin/v1/accounts…`, docs/ADMIN-API.md § Accounts).

use crate::AppState;

pub(crate) fn routes() -> axum::Router<AppState> {
    axum::Router::new()
}
