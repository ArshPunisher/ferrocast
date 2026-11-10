//! Plugins (crates/engine/src/plugins.rs): list, install a plugin file, turn one on or off,
//! remove one. On this PC only: a phone can't add code (remote_scope).

use axum::extract::multipart::MultipartRejection;
use axum::extract::{Multipart, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router, routing::get, routing::put};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;

/// Upload body limit: the biggest plugin file and room for the form around it.
pub const MAX_PLUGIN_BODY: usize = ferrocast_engine::plugins::MAX_ZIP as usize + 64 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/plugins", get(list))
        .route("/plugins/{id}", put(set_on).delete(remove))
        .route("/plugins/{id}/panel", get(panel))
        .route("/plugins/{id}/act", axum::routing::post(act))
}

/// `GET /plugins/{id}/panel` → `{ html, actions }`: a panel plugin's page, for a sandboxed frame.
async fn panel(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.plugin_panel(&id).await?))
}

/// `POST /plugins/{id}/act` with a step → 204, when the panel's plugin is on and may do it.
async fn act(State(app): State<AppState>, Path(id): Path<String>, Body(step): Body<Value>) -> ApiResult<StatusCode> {
    app.engine.plugin_act(&id, step).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.plugins().await))
}

/// `POST /plugins`: multipart `file` (the plugin's `.zip`) → 201 the plugin, installed and on.
pub async fn install(
    State(app): State<AppState>,
    form: Result<Multipart, MultipartRejection>,
) -> ApiResult<impl IntoResponse> {
    use tokio::io::AsyncWriteExt;
    let mut form = form.map_err(|_| {
        ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "wrong_content_type",
            "Send the plugin as multipart/form-data in a field named \"file\".",
        )
    })?;
    let mut field = loop {
        match form.next_field().await {
            Ok(Some(f)) if f.name() == Some("file") => break f,
            Ok(Some(_)) => continue,
            Ok(None) => return Err(ApiError::invalid("Attach the plugin's .zip in a form field named \"file\".")),
            Err(e) => return Err(ApiError::invalid(format!("The upload didn't arrive whole: {}", e.body_text()))),
        }
    };
    let io = |e: std::io::Error| ApiError::from(ferrocast_store::StoreError::Io(e));
    let dir = app.engine.store().dir().join("tmp");
    tokio::fs::create_dir_all(&dir).await.map_err(io)?;
    let path = dir.join(format!("plugin-{}.zip", ferrocast_store::new_id()));
    let mut out = tokio::fs::File::create(&path).await.map_err(io)?;
    let mut size: u64 = 0;
    let copied: ApiResult<()> = async {
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|e| ApiError::invalid(format!("The upload didn't arrive whole: {}", e.body_text())))?
        {
            size += chunk.len() as u64;
            if size > ferrocast_engine::plugins::MAX_ZIP {
                return Err(ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "plugin_too_big",
                    format!("Plugins are up to {} KB.", ferrocast_engine::plugins::MAX_ZIP / 1024),
                )
                .hint("Ask its maker for a smaller one."));
            }
            out.write_all(&chunk).await.map_err(io)?;
        }
        out.flush().await.map_err(io)?;
        Ok(())
    }
    .await;
    drop(out);
    if let Err(e) = copied {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(e);
    }
    if size == 0 {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(ApiError::invalid("The file is empty.").hint("Pick the plugin's .zip file."));
    }
    let result = app.engine.install_plugin(&path).await;
    let _ = tokio::fs::remove_file(&path).await;
    Ok((StatusCode::CREATED, Json(json!(result?))))
}

#[derive(Deserialize)]
struct OnBody {
    on: bool,
}

async fn set_on(State(app): State<AppState>, Path(id): Path<String>, Body(b): Body<OnBody>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.set_plugin_on(&id, b.on).await?)))
}

async fn remove(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.remove_plugin(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}
