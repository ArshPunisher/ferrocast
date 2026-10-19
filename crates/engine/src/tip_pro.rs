//! Pro tip page branding and goal (API.md §9.2): the engine passes the dashboard's calls to
//! the relay with this install's identity. The relay checks Pro, stores the colours, images
//! and goal, and draws the public pages; nothing here is kept on the PC.

use reqwest::Method;
use serde_json::Value;

use crate::{Engine, EngineError};

/// `avatar` or `banner`.
pub const TIP_IMAGES: [&str; 2] = ["avatar", "banner"];

fn image_path(kind: &str) -> Result<String, EngineError> {
    if TIP_IMAGES.contains(&kind) {
        Ok(format!("/v1/pro/tips/brand/{kind}"))
    } else {
        Err(EngineError::Missing {
            code: "not_found",
            message: "There's no such tip page image.",
            hint: "Use avatar or banner.",
        })
    }
}

impl Engine {
    /// The colours and image links (`GET /v1/pro/tips/brand`).
    pub async fn tip_brand(&self) -> Result<Value, EngineError> {
        self.pro_call(Method::GET, "/v1/pro/tips/brand", None).await
    }

    /// Saves `{accent, background, text}`; the answer lists any contrast fixes.
    pub async fn save_tip_brand(&self, body: Value) -> Result<Value, EngineError> {
        self.pro_call(Method::PUT, "/v1/pro/tips/brand", Some(body)).await
    }

    /// Back to the default colours (the images stay).
    pub async fn reset_tip_brand(&self) -> Result<(), EngineError> {
        self.pro_raw(Method::DELETE, "/v1/pro/tips/brand", None).await.map(|_| ())
    }

    /// Uploads an avatar or banner (PNG, JPEG or WebP); the relay re-encodes it.
    pub async fn save_tip_image(&self, kind: &str, content_type: &str, bytes: Vec<u8>) -> Result<Value, EngineError> {
        let path = image_path(kind)?;
        self.pro_raw(Method::PUT, &path, Some((content_type, bytes))).await?.ok_or_else(|| {
            EngineError::Unavailable("The relay saved the image but didn't say where. Reload Tips.".into())
        })
    }

    pub async fn delete_tip_image(&self, kind: &str) -> Result<(), EngineError> {
        let path = image_path(kind)?;
        self.pro_raw(Method::DELETE, &path, None).await.map(|_| ())
    }

    /// The goal with what it raised, or 404 `not_found` without one.
    pub async fn tip_goal(&self) -> Result<Value, EngineError> {
        self.pro_call(Method::GET, "/v1/pro/tips/goal", None).await
    }

    /// Saves `{title, target, currency, ends_at?}`.
    pub async fn save_tip_goal(&self, body: Value) -> Result<Value, EngineError> {
        self.pro_call(Method::PUT, "/v1/pro/tips/goal", Some(body)).await
    }

    pub async fn delete_tip_goal(&self) -> Result<(), EngineError> {
        self.pro_raw(Method::DELETE, "/v1/pro/tips/goal", None).await.map(|_| ())
    }
}

/// Pro's own tip address (API.md §9.2, "Your own address"): the relay adds it to Cloudflare
/// and answers with the DNS record to add and its status; nothing is kept on the PC.
impl Engine {
    /// The address and its status, or 404 `not_found` without one.
    pub async fn tip_domain(&self) -> Result<Value, EngineError> {
        self.pro_call(Method::GET, "/v1/pro/tips/domain", None).await
    }

    /// Adds `{hostname}`; the answer has the CNAME and TXT records to add. The relay's status
    /// comes back too: 201 for a new address, 200 when it's the one this tip page has, and
    /// 409 `domain_taken` with `proof` (the TXT record that would let this PC adopt a
    /// hostname Cloudflare already has), passed on as the error body plus that record.
    pub async fn save_tip_domain(&self, body: Value) -> Result<(u16, Value), EngineError> {
        let (status, v) = self
            .pro_call_passing(Method::PUT, "/v1/pro/tips/domain", Some(body), |s, v| s == 409 && v["proof"].is_object())
            .await?;
        if status != 409 {
            return Ok((status, v));
        }
        // Only the known fields, as strings: nothing else from the relay's answer goes on.
        let t = |p: &str| v.pointer(p).and_then(Value::as_str).unwrap_or("").to_string();
        Ok((
            409,
            serde_json::json!({
                "error": { "code": "domain_taken", "message": t("/error/message"), "hint": t("/error/hint") },
                "proof": { "type": "TXT", "name": t("/proof/name"), "host": t("/proof/host"), "value": t("/proof/value") },
            }),
        ))
    }

    /// Removes it from Cloudflare and the relay (the usual tip link keeps working).
    pub async fn delete_tip_domain(&self) -> Result<(), EngineError> {
        self.pro_raw(Method::DELETE, "/v1/pro/tips/domain", None).await.map(|_| ())
    }
}
