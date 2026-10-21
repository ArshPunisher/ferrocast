//! Tips in cloud mode (docs/CLOUD.md §7): an account with no PC gets a **cloud install**, an
//! `installs` row whose secret nobody holds, linked to the account (`cloud_accounts.install_id`).
//! The existing tip page (`/t/<name>`) and provider webhooks (`/v1/tips/<provider>/<install>`)
//! then work unchanged, and a verified tip goes to the account's bus (`ingest::tip`), never to
//! a delivery queue (no PC reads it).
//!
//! The web dashboard's Tips page talks to `/c/api/v1/tips` with the engine's own shape
//! (API.md "Tips": `TipSettings`), so the same screen works in both places.

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};

use super::{body, json, no_store, typed, web_account};
use crate::tips::TipConfig;
use crate::{ApiError, AppState};

pub(super) fn routes() -> Router<AppState> {
    Router::new().route("/c/api/v1/tips", get(get_tips).put(put_tips).delete(delete_tips))
}

/// The account's cloud install, if it has one.
pub(crate) fn install_of(st: &AppState, account: &str) -> Result<Option<String>, ApiError> {
    Ok(st.db.cloud_account(account).map_err(ApiError::db)?.and_then(|c| c.install_id))
}

/// The account's cloud install, made on first use. Its secret is thrown away at once: the
/// install only ever receives provider webhooks, it never signs in.
pub(crate) fn ensure_install(st: &AppState, account: &str) -> Result<String, ApiError> {
    if let Some(id) = install_of(st, account)? {
        return Ok(id);
    }
    let id = crate::random(12);
    st.db.add_install(&id, &crate::hash(&crate::random(32))).map_err(ApiError::db)?;
    // Marked as used, so the weekly sweep of never-used installs leaves it alone.
    st.db.touch_install(&id).map_err(ApiError::db)?;
    if st.db.cloud_set_install(account, &id).map_err(ApiError::db)? {
        tracing::info!("a cloud mode account got its cloud install for tips");
        return Ok(id);
    }
    // Another request made one first: keep that one.
    let _ = st.db.cloud_drop_install(&id);
    install_of(st, account)?.ok_or_else(|| {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "storage", "The tip page couldn't be set up just now.")
            .hint("Try again in a moment.")
    })
}

/// `TipSettings` (API.md "Tips"): the saved page and webhook addresses, never secrets.
fn settings_view(saved: Option<Value>) -> Value {
    match saved {
        Some(v) => {
            let mut out = v["config"].clone();
            if let Some(o) = out.as_object_mut() {
                o.insert("page_url".into(), v["page_url"].clone());
                o.insert("webhooks".into(), v["webhooks"].clone());
            }
            out
        }
        None => json!({ "handle": "", "display_name": "", "message": "", "page_url": null, "webhooks": null,
                        "razorpay": null, "kofi": null, "stripe": null }),
    }
}

/// `GET /c/api/v1/tips`
async fn get_tips(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let saved = install_of(&st, &a.account.id)?.and_then(|i| crate::tips::saved_for(&st, &i));
    Ok(json(settings_view(saved)))
}

/// `PUT /c/api/v1/tips`: the engine's body (`handle`, `display_name`, `message`, and each
/// provider `null` or `{ page_url, page_id?, test_mode?, secret? }`, a blank secret keeping
/// the saved one) → `TipSettings`.
async fn put_tips(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    b: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let cfg: TipConfig = typed(body(&headers, &b)?)?;
    let install = ensure_install(&st, &a.account.id)?;
    let saved = crate::tips::save_for(&st, &install, cfg)?;
    Ok(json(settings_view(Some(saved))))
}

/// `DELETE /c/api/v1/tips` → 204: the page goes down (its name stays held 30 days).
async fn delete_tips(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    if let Some(install) = install_of(&st, &a.account.id)? {
        st.db.delete_tip_config(&install).map_err(ApiError::db)?;
    }
    Ok(no_store(StatusCode::NO_CONTENT.into_response()))
}
