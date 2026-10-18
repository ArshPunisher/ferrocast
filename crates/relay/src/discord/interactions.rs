//! `POST /discord/interactions`: Discord's interactions endpoint (the developer portal's
//! "Interactions Endpoint URL"). Every request is signed with Ed25519: the signature in
//! `X-Signature-Ed25519` covers `X-Signature-Timestamp` followed by the raw body, checked
//! with the app's public key (`DISCORD_PUBLIC_KEY`). Anything unsigned, signed wrongly or
//! older than five minutes is a 401, which is also what Discord's own check of the URL expects.
//!
//! Today it answers PING (Discord's check). Slash commands (`/ferrocast link` for Kick) are a
//! later step: until then a command gets a short private ("ephemeral") reply.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use ed25519_dalek::{Signature, VerifyingKey};
use serde_json::{Value, json};

use crate::{ApiError, AppState, now_ms};

/// Signed requests older (or newer) than this are refused: a captured one can't be replayed later.
pub(crate) const MAX_SKEW_S: i64 = 300;

/// The app's public key from the portal: 64 hex characters.
pub(crate) fn parse_public_key(hex: &str) -> Option<VerifyingKey> {
    let bytes: [u8; 32] = decode_hex(hex.trim())?.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

/// Strict hex: an even number of `0-9a-fA-F` only (`from_str_radix` alone would take a `+`).
pub(crate) fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// Whether Discord signed `timestamp + body`, and the timestamp is recent.
pub(crate) fn verify(key: &VerifyingKey, signature_hex: &str, timestamp: &str, body: &[u8], now_s: i64) -> bool {
    let Ok(ts) = timestamp.trim().parse::<i64>() else { return false };
    if (now_s - ts).abs() > MAX_SKEW_S {
        return false;
    }
    let Some(sig) = decode_hex(signature_hex.trim()).and_then(|b| <[u8; 64]>::try_from(b).ok()) else {
        return false;
    };
    let mut msg = Vec::with_capacity(timestamp.len() + body.len());
    msg.extend_from_slice(timestamp.as_bytes());
    msg.extend_from_slice(body);
    key.verify_strict(&msg, &Signature::from_bytes(&sig)).is_ok()
}

const PING: i64 = 1;
const PONG: i64 = 1;
const CHANNEL_MESSAGE: i64 = 4;
const EPHEMERAL: i64 = 1 << 6;

pub(crate) async fn interactions(State(st): State<AppState>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let Some(key) = st.discord.public_key() else {
        return super::not_configured().into_response();
    };
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    if !verify(&key, &h("x-signature-ed25519"), &h("x-signature-timestamp"), &body, now_ms() / 1000) {
        return ApiError::new(StatusCode::UNAUTHORIZED, "bad_signature", "The request isn't signed by Discord.")
            .into_response();
    }
    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
        return ApiError::new(StatusCode::BAD_REQUEST, "bad_json", "The interaction isn't JSON.").into_response();
    };
    if v["type"].as_i64() == Some(PING) {
        return axum::Json(json!({ "type": PONG })).into_response();
    }
    // A command, a button, an autocomplete: none exist yet. A private reply, never a post to
    // the channel (R-06: nothing Pro-related ever shows to the audience).
    axum::Json(json!({
        "type": CHANNEL_MESSAGE,
        "data": { "content": "Ferrocast's commands aren't ready yet.", "flags": EPHEMERAL, "allowed_mentions": { "parse": [] } },
    }))
    .into_response()
}
