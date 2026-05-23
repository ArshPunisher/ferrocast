//! Kick's side of the relay: webhook verification and the OAuth broker.

use std::sync::RwLock;
use std::time::{Duration, Instant};

use base64::Engine as _;
use rsa::RsaPublicKey;
use rsa::pkcs1v15::{Signature, VerifyingKey};
use rsa::pkcs8::DecodePublicKey;
use rsa::sha2::Sha256;
use rsa::signature::Verifier;
use serde::Deserialize;
use serde_json::Value;

/// Webhooks older or newer than this are refused (replay window).
pub const TIMESTAMP_WINDOW_MS: i64 = 10 * 60 * 1000;

/// Kick's public key, fetched from Kick (never hardcoded: it may rotate) and refreshed
/// when a signature fails and the cached key is more than a minute old.
#[derive(Debug, Default)]
pub struct KeyCache {
    key: RwLock<Option<(RsaPublicKey, Instant)>>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VerifyError {
    #[error("missing header {0}")]
    MissingHeader(&'static str),
    #[error("timestamp outside the allowed window")]
    Stale,
    #[error("bad signature")]
    BadSignature,
    #[error("couldn't get Kick's public key: {0}")]
    Key(String),
}

pub fn parse_key(pem: &str) -> Result<RsaPublicKey, VerifyError> {
    RsaPublicKey::from_public_key_pem(pem.trim()).map_err(|e| VerifyError::Key(e.to_string()))
}

/// `true` if `sig_b64` signs `id.timestamp.body` with `key` (RSA PKCS#1 v1.5, SHA-256).
pub fn signature_ok(key: &RsaPublicKey, message_id: &str, timestamp: &str, body: &[u8], sig_b64: &str) -> bool {
    let Ok(sig_bytes) = base64::engine::general_purpose::STANDARD.decode(sig_b64.trim()) else { return false };
    let Ok(sig) = Signature::try_from(sig_bytes.as_slice()) else { return false };
    let mut signed = Vec::with_capacity(message_id.len() + timestamp.len() + body.len() + 2);
    signed.extend_from_slice(message_id.as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(timestamp.as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(body);
    VerifyingKey::<Sha256>::new(key.clone()).verify(&signed, &sig).is_ok()
}

/// Parses an RFC 3339 timestamp to Unix milliseconds (seconds precision is plenty for a
/// ten-minute window). Accepts `Z` and `+hh:mm` offsets.
pub fn rfc3339_ms(ts: &str) -> Option<i64> {
    let ts = ts.trim();
    let (date, rest) = ts.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    let (time, offset_s) = if let Some(t) = rest.strip_suffix('Z').or_else(|| rest.strip_suffix('z')) {
        (t, 0)
    } else {
        let pos = rest.rfind(['+', '-'])?;
        let (t, off) = rest.split_at(pos);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let (oh, om) = off[1..].split_once(':')?;
        (t, sign * (oh.parse::<i64>().ok()? * 3600 + om.parse::<i64>().ok()? * 60))
    };
    let time = time.split('.').next()?;
    let mut t = time.split(':');
    let (hh, mm, ss): (i64, i64, i64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?, t.next()?.parse().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let yy = if m <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hh * 3600 + mm * 60 + ss - offset_s) * 1000)
}

impl KeyCache {
    async fn fetch(&self, http: &reqwest::Client, api: &str) -> Result<RsaPublicKey, VerifyError> {
        let v: Value = http
            .get(format!("{api}/public/v1/public-key"))
            .send()
            .await
            .map_err(|e| VerifyError::Key(e.to_string()))?
            .json()
            .await
            .map_err(|e| VerifyError::Key(e.to_string()))?;
        let pem = v
            .pointer("/data/public_key")
            .and_then(Value::as_str)
            .ok_or_else(|| VerifyError::Key("no key in the response".into()))?;
        let key = parse_key(pem)?;
        *self.key.write().unwrap_or_else(|p| p.into_inner()) = Some((key.clone(), Instant::now()));
        Ok(key)
    }

    /// Verifies a webhook, refetching the key once if the cached one doesn't match.
    pub async fn verify(
        &self,
        http: &reqwest::Client,
        api: &str,
        d: &Delivery<'_>,
        now_ms: i64,
    ) -> Result<(), VerifyError> {
        let Delivery { message_id, timestamp, body, signature } = *d;
        let at = rfc3339_ms(timestamp).ok_or(VerifyError::Stale)?;
        if (now_ms - at).abs() > TIMESTAMP_WINDOW_MS {
            return Err(VerifyError::Stale);
        }
        let cached = self.key.read().unwrap_or_else(|p| p.into_inner()).clone();
        let (key, fetched) = match cached {
            Some(k) => k,
            None => (self.fetch(http, api).await?, Instant::now()),
        };
        if signature_ok(&key, message_id, timestamp, body, signature) {
            return Ok(());
        }
        if fetched.elapsed() > Duration::from_secs(60) {
            let fresh = self.fetch(http, api).await?;
            if signature_ok(&fresh, message_id, timestamp, body, signature) {
                return Ok(());
            }
        }
        Err(VerifyError::BadSignature)
    }
}

/// The signed parts of one webhook delivery.
#[derive(Debug, Clone, Copy)]
pub struct Delivery<'a> {
    pub message_id: &'a str,
    pub timestamp: &'a str,
    pub body: &'a [u8],
    pub signature: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    /// Kick has documented this as a string in places; accept both.
    #[serde(deserialize_with = "number_or_string")]
    pub expires_in: i64,
}

fn number_or_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(match v {
        Value::Number(n) => n.as_i64().unwrap_or(3600),
        Value::String(s) => s.parse().unwrap_or(3600),
        _ => 3600,
    })
}
