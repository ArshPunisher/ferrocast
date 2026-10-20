//! The web dashboard's account calls on the web host (docs/ACCOUNTS.md §6, §7): the
//! account's PCs, opening one without a pairing code, and Pro checkout from the web.

use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::*;

pub(super) fn routes() -> axum::Router<AppState> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/r/v1/pcs", get(pcs))
        .route("/r/v1/pcs/{id}/connect", post(connect))
        .route("/r/v1/pro/checkout", post(checkout))
}

/// A PC's id for the web: stable, not the install id.
pub(crate) fn pc_id(install: &str) -> String {
    format!("pc_{}", &sha(&format!("ferrocast pc v1|{install}"))[..20])
}

/// `GET /r/v1/pcs`
async fn pcs(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_session(&st, &headers)?;
    let now = now_ms();
    let list = st.db.account_pcs(&a.account.id, now).map_err(ApiError::db)?;
    let mut out = Vec::new();
    for (install, label, last) in list {
        let pro = st
            .db
            .pro_sub_for_install(&install)
            .map_err(ApiError::db)?
            .is_some_and(|s| access::is_pro(&s.access(), now));
        out.push(json!({
            "id": pc_id(&install), "label": label, "pc_online": st.remote.pc_online(&install),
            "pro": pro, "last_seen_at": last,
        }));
    }
    Ok(no_store(axum::Json(json!({ "pcs": out })).into_response()))
}

/// `POST /r/v1/pcs/{id}/connect {name?}` → 201 like `/r/v1/pair`.
async fn connect(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let a = web_session_write(&st, &headers)?;
    let b = json_body(&headers, &body)?;
    let now = now_ms();
    let install = st
        .db
        .account_pcs(&a.account.id, now)
        .map_err(ApiError::db)?
        .into_iter()
        .map(|(i, _, _)| i)
        .find(|i| pc_id(i) == id)
        .ok_or_else(|| e(StatusCode::NOT_FOUND, "not_found", "No PC of yours with that id."))?;
    if let Err(wait) = st.accounts.take("connect", &a.account.id, 30, 3_600_000) {
        return Err(rate_limited(wait, "That's a lot of new dashboards in an hour."));
    }
    let name = match &b["name"] {
        Value::String(s) => clean_label(s),
        _ => String::new(),
    };
    let name = if name.is_empty() { ua_label(&headers) } else { name };
    let v = crate::remote::connect_owner(&st, &install, &a.account.id, &name)?;
    Ok(no_store((StatusCode::CREATED, axum::Json(v)).into_response()))
}

/// `POST /r/v1/pro/checkout {plan, code?}`: checkout for the account (its verified email).
async fn checkout(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let a = web_session_write(&st, &headers)?;
    let owner = crate::billing::Owner::Account { id: a.account.id.clone(), email: a.account.email.clone() };
    crate::billing::checkout_as(&st, owner, peer, &headers, &body).await
}
