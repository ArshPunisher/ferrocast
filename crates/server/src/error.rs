//! Every error the API returns: the right status and `{error:{code,message,hint}}` with a
//! plain message (API.md §1). Nothing falls through to a bare 500.

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ferrocast_connector::ConnectorError;
use ferrocast_engine::EngineError;
use ferrocast_store::StoreError;
use serde_json::json;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub hint: Option<String>,
    pub retry_after: Option<u64>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        ApiError {
            status,
            code,
            message: message.into(),
            hint: None,
            retry_after: None,
        }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        ApiError::new(StatusCode::BAD_REQUEST, "invalid", message)
    }

    pub fn not_found(what: &str) -> Self {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("That {what} doesn't exist."),
        )
        .hint("Reload the page; it may have been deleted.")
    }

    pub fn unauthorized() -> Self {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "This dashboard isn't signed in to the app.",
        )
        .hint("Open the dashboard from the Ferrocast app, or pair this browser again.")
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        ApiError::new(StatusCode::FORBIDDEN, "forbidden", message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "unavailable", message)
    }

    pub fn too_many(seconds: u64) -> Self {
        let mut e = ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Too many tries. Wait a moment.",
        );
        e.retry_after = Some(seconds);
        e
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body =
            json!({ "error": { "code": self.code, "message": self.message, "hint": self.hint } });
        let mut res = (self.status, Json(body)).into_response();
        if let Some(s) = self.retry_after
            && let Ok(v) = HeaderValue::from_str(&s.to_string())
        {
            res.headers_mut().insert(header::RETRY_AFTER, v);
        }
        res
    }
}

impl From<EngineError> for ApiError {
    fn from(e: EngineError) -> Self {
        match e {
            EngineError::Invalid(m) => ApiError::invalid(m),
            EngineError::Unavailable(m) => ApiError::unavailable(m),
            EngineError::NotFound(what) => ApiError::not_found(what),
            EngineError::Connector(c) => c.into(),
            EngineError::Store(s) => s.into(),
            EngineError::Internal(m) => {
                tracing::error!(error = %m, "internal error");
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "Something went wrong inside Ferrocast.",
                )
                .hint("Try again. If it keeps happening, use Report a problem in Settings.")
            }
        }
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound(what) => ApiError::not_found(what),
            StoreError::Conflict(m) => ApiError::new(StatusCode::CONFLICT, "conflict", m),
            other => {
                tracing::error!(error = %other, "storage error");
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "storage",
                    "Ferrocast couldn't read or save its data.",
                )
                .hint("Check there's free disk space, then try again.")
            }
        }
    }
}

impl From<ConnectorError> for ApiError {
    fn from(e: ConnectorError) -> Self {
        let message = e.to_string();
        match e {
            ConnectorError::RateLimited { retry_after, .. } => {
                let mut a = ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "platform_rate_limited",
                    message,
                );
                a.retry_after = Some(retry_after.as_secs().max(1));
                a
            }
            ConnectorError::NotConnected(_) => {
                ApiError::unavailable(message).hint("Connect the account in Settings → Accounts.")
            }
            ConnectorError::SignInNeeded(_) | ConnectorError::MissingScope { .. } => {
                ApiError::unavailable(message).hint("Reconnect the account in Settings → Accounts.")
            }
            ConnectorError::NotSupported { .. } | ConnectorError::Refused { .. } => {
                ApiError::unavailable(message)
            }
            ConnectorError::Network { .. } => {
                ApiError::new(StatusCode::BAD_GATEWAY, "platform_unreachable", message)
                    .hint("Check your connection and try again.")
            }
        }
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
