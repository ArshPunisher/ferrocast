//! Pro's weekly email (API.md §9.8): the engine passes the dashboard's calls to the relay with
//! this install's identity. The relay keeps the address (sealed), sends the confirm email and
//! the weekly report; nothing about it is kept on the PC.

use reqwest::Method;
use serde_json::{Value, json};

use crate::{Engine, EngineError};

const PATH: &str = "/v1/pro/history/email";

impl Engine {
    /// `{ status, address_hint, … }` from the relay.
    pub async fn weekly_email(&self) -> Result<Value, EngineError> {
        self.pro_call(Method::GET, PATH, None).await
    }

    /// Saves an address and sends its confirm email (the relay answers `pending`).
    pub async fn save_weekly_email(&self, address: &str) -> Result<Value, EngineError> {
        self.pro_call(Method::PUT, PATH, Some(json!({ "address": address }))).await
    }

    /// Stops the emails and deletes the address on the relay.
    pub async fn stop_weekly_email(&self) -> Result<(), EngineError> {
        self.pro_raw(Method::DELETE, PATH, None).await.map(|_| ())
    }
}
