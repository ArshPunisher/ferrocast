//! Discord go-live posts: settings (the webhook stays in the keychain) and a test post.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;

pub async fn get(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.discord().await?)))
}

/// `{ webhook_url?: string | null, enabled?, template?, mention_everyone? }`: only what's
/// sent changes. The response never contains the webhook URL.
pub async fn save(State(app): State<AppState>, Body(patch): Body<Value>) -> ApiResult<Json<Value>> {
    let Some(patch) = patch.as_object() else {
        return Err(ApiError::invalid("The Discord settings must be an object."));
    };
    Ok(Json(json!(app.engine.save_discord(patch).await?)))
}

pub async fn test(State(app): State<AppState>) -> ApiResult<StatusCode> {
    match app.engine.test_discord().await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(f) => {
            let mut e = ApiError::from(f.error);
            if let Some(s) = f.retry_after {
                e.retry_after = Some(s);
            }
            Err(e)
        }
    }
}
