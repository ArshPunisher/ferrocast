//! A JSON body extractor whose failures use our error shape instead of axum's plain text.

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Request};
use serde::de::DeserializeOwned;

use crate::error::ApiError;

#[derive(Debug)]
pub struct Body<T>(pub T);

impl<S, T> FromRequest<S> for Body<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(v)) => Ok(Body(v)),
            Err(rej) => Err(match rej {
                JsonRejection::MissingJsonContentType(_) => ApiError::new(
                    axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "wrong_content_type",
                    "Send JSON with the header Content-Type: application/json.",
                ),
                JsonRejection::JsonSyntaxError(_) => {
                    ApiError::invalid("The request isn't valid JSON.")
                }
                JsonRejection::JsonDataError(e) => {
                    ApiError::invalid(format!("A field is missing or wrong: {}", e.body_text()))
                }
                JsonRejection::BytesRejection(_) => ApiError::new(
                    axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                    "too_large",
                    format!(
                        "The request is too big; the limit is {} KB.",
                        crate::JSON_LIMIT / 1024
                    ),
                ),
                other => ApiError::invalid(other.body_text()),
            }),
        }
    }
}
