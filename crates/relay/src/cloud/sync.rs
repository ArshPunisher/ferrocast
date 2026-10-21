//! The desktop app's side of cloud mode (docs/CLOUD.md §1.1, §9.3): its `fca_` session,
//! documents pulled since a version and pushed with the version they started from. A
//! document that moved on in the cloud is a conflict, never overwritten: the cloud copy wins
//! and the app keeps its own as a copy.

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};

use super::store::{Doc, KINDS, Write};
use super::{app_account, body, docs, json, typed};
use crate::{ApiError, AppState, now_ms};

/// Documents per page of changes, and per push.
pub const PAGE: i64 = 500;
pub const MAX_PUSH: usize = 100;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/cloud", get(summary))
        .route("/v1/cloud/enable", post(enable))
        .route("/v1/cloud/docs", get(pull).put(push))
        .route("/v1/cloud/overlays", get(overlays))
}

async fn summary(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = app_account(&st, &headers, &method)?;
    Ok(json(super::web::summary_json(&st, &a.account.id)?))
}

async fn enable(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = app_account(&st, &headers, &method)?;
    if st.db.cloud_enable(&a.account.id, now_ms()).map_err(ApiError::db)? {
        docs::seed(&st, &a.account.id)?;
    }
    Ok(json(super::web::summary_json(&st, &a.account.id)?))
}

fn doc_json(d: &Doc) -> Value {
    json!({
        "kind": d.kind, "id": d.id, "version": d.version, "updated_at": d.updated_at,
        "deleted": d.deleted_at.is_some(),
        "body": d.json.as_deref().and_then(|j| serde_json::from_str::<Value>(j).ok()),
    })
}

/// `GET /v1/cloud/docs?since=N`: every document changed after N, deletions included.
async fn pull(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let a = app_account(&st, &headers, &method)?;
    let since = match q.get("since").map(|s| s.trim()).filter(|s| !s.is_empty()) {
        None => 0,
        Some(s) => s.parse::<i64>().map_err(|_| {
            super::invalid("since: a whole number, the highest version this PC has seen (0 for everything).")
        })?,
    };
    let c = super::must_be_on(&st, &a.account.id)?;
    let list = st.db.cloud_changes(&a.account.id, since.max(0), PAGE + 1).map_err(ApiError::db)?;
    let more = list.len() as i64 > PAGE;
    let docs: Vec<Value> = list.iter().take(PAGE as usize).map(doc_json).collect();
    // With more to come, the version to ask from next is the last one sent.
    let version = if more { list.get(PAGE as usize - 1).map_or(c.version, |d| d.version) } else { c.version };
    Ok(json(json!({ "version": version, "more": more, "docs": docs })))
}

#[derive(Deserialize)]
struct PushDoc {
    kind: String,
    id: String,
    base_version: i64,
    #[serde(default)]
    body: Option<Value>,
}

#[derive(Deserialize)]
struct Push {
    docs: Vec<PushDoc>,
}

/// `PUT /v1/cloud/docs`: each document only where the cloud copy is still at its base
/// version; the rest come back as conflicts with the cloud's copy (200, not an error).
async fn push(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    b: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let a = app_account(&st, &headers, &method)?;
    let account = a.account.id.clone();
    super::must_be_on(&st, &account)?;
    let p: Push = typed(body(&headers, &b)?)?;
    if p.docs.len() > MAX_PUSH {
        return Err(super::e(
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            "too_many",
            format!("Send at most {MAX_PUSH} documents at once."),
        ));
    }
    // Every document is checked before anything is written, so a bad one changes nothing.
    let mut checked = Vec::with_capacity(p.docs.len());
    for (i, d) in p.docs.iter().enumerate() {
        if !KINDS.contains(&d.kind.as_str()) {
            return Err(super::invalid(format!("docs[{i}].kind: use overlay, alert_rule, design or settings.")));
        }
        if d.id.is_empty() || d.id.len() > 80 || d.id.chars().any(|c| c.is_control() || c == '/') {
            return Err(super::invalid(format!("docs[{i}].id: 1 to 80 characters, no slashes.")));
        }
        let json = match &d.body {
            Some(v) => Some(
                docs::check_pushed(&st, &account, &d.kind, &d.id, v)
                    .map_err(|m| super::invalid(format!("docs[{i}]: {m}")))?,
            ),
            None => None,
        };
        if json.as_ref().is_some_and(|j| j.len() > super::MAX_DOC_BYTES) {
            return Err(super::invalid(format!("docs[{i}]: over 256 KB.")));
        }
        checked.push((d, json));
    }
    // The per-kind caps, counted for the whole push before anything is written.
    let mut new_per_kind: std::collections::HashMap<&str, i64> = Default::default();
    for (d, j) in &checked {
        let exists =
            st.db.cloud_doc(&account, &d.kind, &d.id).map_err(ApiError::db)?.is_some_and(|x| x.deleted_at.is_none());
        if j.is_some() && !exists {
            *new_per_kind.entry(d.kind.as_str()).or_default() += 1;
        }
    }
    for (kind, n) in new_per_kind {
        if docs::over_cap(&st, &account, kind, n)? {
            return Err(super::limit(format!("Cloud mode is full for {}s.", kind.replace('_', " ")))
                .hint("Delete some in the web dashboard, then sync again."));
        }
    }
    let mut applied = Vec::new();
    let mut conflicts = Vec::new();
    let now = now_ms();
    for (d, j) in checked {
        match st.db.cloud_write(&account, &d.kind, &d.id, j.as_deref(), Some(d.base_version), now).map_err(ApiError::db)? {
            Write::Saved(v) => {
                applied.push(json!({ "kind": d.kind, "id": d.id, "version": v }));
                if d.kind == docs::OVERLAY {
                    match j.as_deref().and_then(|j| serde_json::from_str(j).ok()) {
                        Some(o) => super::changed_overlay(&st, &account, &o),
                        None => {
                            // Retire the URL but keep its generation, so no old URL comes back.
                            if let Err(e) = super::retire_url(&st, &account, &d.id) {
                                tracing::warn!(error = e.message(), "couldn't retire a deleted overlay's URL");
                            }
                            super::revoked_overlay(&st, &account, &d.id);
                        }
                    }
                }
            }
            Write::Conflict(server) => conflicts.push(json!({
                "kind": d.kind, "id": d.id,
                "server": server.as_ref().map_or_else(|| json!({ "version": 0, "deleted": true, "body": null }), doc_json),
            })),
        }
    }
    let version = st.db.cloud_account(&account).map_err(ApiError::db)?.map_or(0, |c| c.version);
    Ok(json(json!({ "version": version, "applied": applied, "conflicts": conflicts })))
}

/// `GET /v1/cloud/overlays`: the cloud overlays with their URLs ("Use cloud overlays").
async fn overlays(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = app_account(&st, &headers, &method)?;
    super::must_be_on(&st, &a.account.id)?;
    let id = &a.account.id;
    let views =
        docs::overlays(&st, id)?.iter().map(|o| docs::overlay_view(&st, id, o)).collect::<Result<Vec<_>, _>>()?;
    Ok(json(json!(views)))
}
