//! The one check every Pro feature route makes first: a known install (401) whose
//! subscription grants Pro right now (402). Access comes from [`access::is_pro`], the same
//! rule `/v1/pro/status` answers with (paid period, late-renewal grace, refunds), so a
//! feature and Settings › Pro can never disagree.

use axum::http::{HeaderMap, StatusCode};

use super::access;
use crate::{ApiError, AppState, authed, now_ms};

/// Who a Pro request is for. Quotas and stored data belong to `sub_id` (shared by up to
/// three installs); `install` is the PC that asked.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // the first Pro feature routes arrive in M1
pub(crate) struct ProCtx {
    pub install: String,
    pub sub_id: String,
}

/// 402 `pro_required`: the install is fine, it just doesn't have Pro (never had it, it
/// lapsed, or it was refunded).
#[allow(dead_code)] // the first Pro feature routes arrive in M1
pub(crate) fn pro_required() -> ApiError {
    ApiError::new(StatusCode::PAYMENT_REQUIRED, "pro_required", "This runs on Ferrocast's servers and is part of Pro.")
        .hint("See Settings › Pro. Everything free in Ferrocast stays free.")
}

/// The install behind `Authorization`, if its subscription grants Pro now.
#[allow(dead_code)] // the first Pro feature routes arrive in M1
pub(crate) async fn require_pro(st: &AppState, headers: &HeaderMap) -> Result<ProCtx, ApiError> {
    let install = authed(st, headers)?;
    let sub = st.db.pro_sub_for_install(&install).map_err(ApiError::db)?;
    match sub {
        Some(s) if access::is_pro(&s.access(), now_ms()) => Ok(ProCtx { install, sub_id: s.id }),
        _ => Err(pro_required()),
    }
}
