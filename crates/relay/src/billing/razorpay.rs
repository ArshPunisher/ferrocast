//! A small Razorpay API client and the webhook signature check. Only what Pro uses:
//! plans, subscriptions, cancelling, and a payment's refunds.
//! Docs: <https://razorpay.com/docs/api/payments/subscriptions/>

use std::time::Duration;

use hmac::{Hmac, Mac};
use rsa::sha2::Sha256;
use serde_json::{Value, json};

/// `X-Razorpay-Signature` = hex HMAC-SHA256 of the raw body with the webhook secret.
/// Compared in constant time; an empty secret or signature never matches.
pub fn webhook_signature_ok(secret: &str, body: &[u8], signature: &str) -> bool {
    let signature = signature.trim();
    if secret.is_empty() || signature.is_empty() {
        return false;
    }
    let Some(sig) = hex(signature) else { return false };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else { return false };
    mac.update(body);
    // `verify_slice` compares in constant time.
    mac.verify_slice(&sig).is_ok()
}

/// A SHA-256 hex digest (64 hex digits, either case) as bytes.
fn hex(s: &str) -> Option<Vec<u8>> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| s.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok())).collect()
}

/// Signs like Razorpay does (tests, and the fake's webhook sender).
pub fn sign(secret: &str, body: &[u8]) -> String {
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else { return String::new() };
    mac.update(body);
    mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// Razorpay refused a request, or couldn't be reached, or sent something unusable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RzpError {
    #[error("Razorpay couldn't be reached: {0}")]
    Unreachable(String),
    /// Razorpay's own status and error description.
    #[error("Razorpay said {0}: {1}")]
    Refused(u16, String),
    #[error("Razorpay sent an answer the relay can't read")]
    BadAnswer,
}

#[derive(Clone)]
pub struct Razorpay {
    http: reqwest::Client,
    base: String,
    key_id: String,
    key_secret: String,
}

impl std::fmt::Debug for Razorpay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Razorpay").field("base", &self.base).field("key_id", &self.key_id).finish_non_exhaustive()
    }
}

impl Razorpay {
    pub fn new(base: &str, key_id: &str, key_secret: &str) -> Razorpay {
        Razorpay {
            http: reqwest::Client::builder().timeout(Duration::from_secs(20)).build().unwrap_or_default(),
            base: base.trim_end_matches('/').to_string(),
            key_id: key_id.to_string(),
            key_secret: key_secret.to_string(),
        }
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, RzpError> {
        let mut req =
            self.http.request(method, format!("{}{path}", self.base)).basic_auth(&self.key_id, Some(&self.key_secret));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await.map_err(|e| RzpError::Unreachable(e.without_url().to_string()))?;
        let status = res.status().as_u16();
        let v: Value = res.json().await.map_err(|_| {
            if status >= 400 { RzpError::Refused(status, "no description".into()) } else { RzpError::BadAnswer }
        })?;
        if status >= 400 {
            let d = v.pointer("/error/description").and_then(Value::as_str).unwrap_or("no description");
            return Err(RzpError::Refused(status, d.chars().take(300).collect()));
        }
        Ok(v)
    }

    pub async fn fetch_plan(&self, id: &str) -> Result<Value, RzpError> {
        self.call(reqwest::Method::GET, &format!("/plans/{}", path_id(id)?), None).await
    }

    pub async fn create_subscription(&self, body: Value) -> Result<Value, RzpError> {
        self.call(reqwest::Method::POST, "/subscriptions", Some(body)).await
    }

    pub async fn fetch_subscription(&self, id: &str) -> Result<Value, RzpError> {
        self.call(reqwest::Method::GET, &format!("/subscriptions/{}", path_id(id)?), None).await
    }

    pub async fn cancel_subscription(&self, id: &str, at_cycle_end: bool) -> Result<Value, RzpError> {
        let body = json!({ "cancel_at_cycle_end": i32::from(at_cycle_end) });
        self.call(reqwest::Method::POST, &format!("/subscriptions/{}/cancel", path_id(id)?), Some(body)).await
    }

    /// Every refund on a payment (a payment can have several).
    pub async fn payment_refunds(&self, payment_id: &str) -> Result<Vec<Value>, RzpError> {
        let v = self
            .call(reqwest::Method::GET, &format!("/payments/{}/refunds?count=100", path_id(payment_id)?), None)
            .await?;
        Ok(v["items"].as_array().cloned().unwrap_or_default())
    }
}

/// Ids go into URL paths: only Razorpay's own id characters.
fn path_id(id: &str) -> Result<&str, RzpError> {
    if !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        Ok(id)
    } else {
        Err(RzpError::Refused(400, "that isn't a Razorpay id".into()))
    }
}

/// Is this a well-formed Razorpay id with the given prefix (`sub_`, `pay_`, `plan_`)?
pub fn is_id(id: &str, prefix: &str) -> bool {
    id.starts_with(prefix) && path_id(id).is_ok() && id.len() > prefix.len()
}
