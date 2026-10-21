//! Cloud mode's numbers for the Control Center (docs/CLOUD.md §9.4): `GET /admin/v1/cloud/stats`,
//! behind the same guard as every admin route (the server's own network, the admin token).

use std::sync::atomic::Ordering;

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::{ApiError, AppState, now_ms};

/// Memory one active streamer's bus takes at most, by §8's measure (rings, queue, 3 sockets).
pub const BYTES_PER_ACTIVE: u64 = 300 * 1024;

pub(super) fn routes() -> axum::Router<AppState> {
    axum::Router::new().route("/admin/v1/cloud/stats", axum::routing::get(stats))
}

async fn stats(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    crate::admin::guard(&st, peer, &headers, now_ms())?;
    let body = collect(&st).map_err(|e| {
        tracing::error!(error = %e, "couldn't read the cloud stats");
        crate::admin::unreadable()
    })?;
    let mut res = axum::Json(body).into_response();
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

pub(crate) fn collect(st: &AppState) -> rusqlite::Result<Value> {
    let accounts = st.db.cloud_accounts()?;
    let sleeping = accounts.iter().filter(|a| a.sleeping_at.is_some()).count();
    let mut channels = serde_json::Map::new();
    for p in ["twitch", "kick", "youtube"] {
        channels.insert(p.into(), json!(0));
    }
    for ch in st.db.cloud_all_channels()? {
        let n = channels.get(&ch.platform).and_then(Value::as_u64).unwrap_or(0) + 1;
        channels.insert(ch.platform, json!(n));
    }
    let (files, bytes) = st.db.cloud_media_totals()?;
    let b = &st.cloud.buses;
    let active = b.active();
    Ok(json!({
        "available": super::available(st),
        "active_streamers": active,
        "max_active": st.cloud.cfg.max_active,
        "accounts_on": accounts.len(),
        "sleeping": sleeping,
        "channels": channels,
        "overlay_sockets": b.sockets.load(Ordering::Relaxed),
        "max_sockets": st.cloud.cfg.max_sockets,
        "events_last_hour": b.events_last_hour.load(Ordering::Relaxed),
        "events_since_start": b.events.load(Ordering::Relaxed),
        "dropped_events": b.dropped.load(Ordering::Relaxed),
        "storage": { "bytes": bytes, "files": files },
        "memory_estimate_bytes": active as u64 * BYTES_PER_ACTIVE,
    }))
}
