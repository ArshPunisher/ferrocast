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
