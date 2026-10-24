//! `/api/v1/designs`: the gallery (built-in and own designs), Use, Duplicate, the alert
//! design in use, and `.ferrodesign` export and import (API.md §4 Designs).

use axum::Json;
use axum::extract::multipart::MultipartRejection;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use ferrocast_core::design::{Design, DesignKind};
use ferrocast_engine::designs::UseRequest;
use ferrocast_engine::obs_theme::ThemePlan;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;

/// Largest `.ferrodesign` upload, plus room for the form around it.
pub const MAX_DESIGN_BODY: usize = ferrocast_store::designfile::MAX_FILE as usize + 64 * 1024;

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    kind: Option<String>,
}

fn kind_of(s: &str) -> Result<DesignKind, ApiError> {
    match s {
        "alert" => Ok(DesignKind::Alert),
        "theme" => Ok(DesignKind::Theme),
        "scene" => Ok(DesignKind::Scene),
        "widget" => Ok(DesignKind::Widget),
        _ => Err(ApiError::invalid(format!("kind: \"{s}\" isn't a design kind; use alert, theme, scene or widget."))),
    }
}

pub async fn list(State(app): State<AppState>, Query(q): Query<ListQuery>) -> ApiResult<Json<Value>> {
    let kind = q.kind.as_deref().filter(|k| !k.is_empty()).map(kind_of).transpose()?;
    Ok(Json(json!(app.engine.designs(kind).await?)))
}

pub async fn get(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.design(&id).await?)))
}

pub async fn create(State(app): State<AppState>, Body(d): Body<Design>) -> ApiResult<impl IntoResponse> {
    Ok((StatusCode::CREATED, Json(json!(app.engine.create_design(d).await?))))
}

pub async fn update(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Body(d): Body<Design>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.update_design(&id, d).await?)))
}

pub async fn remove(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.delete_design(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default)]
pub struct DuplicateBody {
    #[serde(default)]
    name: Option<String>,
}

/// A JSON body that may be left out entirely.
fn optional_body<T: serde::de::DeserializeOwned + Default>(bytes: &[u8]) -> Result<T, ApiError> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(bytes).map_err(|e| {
        if e.is_syntax() || e.is_eof() {
            ApiError::invalid("The request isn't valid JSON.")
        } else {
            ApiError::invalid(format!("A field is missing or wrong: {e}"))
        }
    })
}

pub async fn duplicate(
    State(app): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<impl IntoResponse> {
    let name = optional_body::<DuplicateBody>(&body)?.name;
    Ok((StatusCode::CREATED, Json(json!(app.engine.duplicate_design(&id, name.as_deref()).await?))))
}

/// `GET /designs/{id}/obs`: Ferrocast's guesses for putting a theme into OBS.
pub async fn obs_plan(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.theme_obs_plan(&id).await?)))
}

/// `POST /designs/{id}/obs`: puts a theme into OBS (an empty body uses the guesses).
pub async fn put_in_obs(
    State(app): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let plan: Option<ThemePlan> = optional_body(&body)?;
    Ok(Json(json!(app.engine.apply_theme_to_obs(&id, plan).await?)))
}

/// `GET /obs/theme`: the theme Ferrocast put into OBS, or null.
pub async fn obs_theme(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.obs_theme().await?)))
}

/// `DELETE /obs/theme`: takes it out again (only what Ferrocast added).
pub async fn take_theme_out(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.undo_theme_in_obs().await?)))
}

pub async fn use_design(
    State(app): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let req: UseRequest = optional_body(&body)?;
    Ok(Json(json!(app.engine.use_design(&id, req).await?)))
}

pub async fn active_alert(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "design_id": app.engine.active_alert_design().await? })))
}

#[derive(Deserialize)]
pub struct ActiveBody {
    design_id: Option<String>,
}

pub async fn set_active_alert(State(app): State<AppState>, Body(b): Body<ActiveBody>) -> ApiResult<Json<Value>> {
    let id = app.engine.set_active_alert_design(b.design_id.as_deref()).await?;
    Ok(Json(json!({ "design_id": id })))
}

/// `POST /designs/{id}/export`: the design as a `.ferrodesign` download.
pub async fn export(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<axum::response::Response> {
    let dir = app.engine.store().dir().join("tmp");
    tokio::fs::create_dir_all(&dir).await.map_err(|e| ApiError::from(ferrocast_store::StoreError::Io(e)))?;
    let path = dir.join(format!("export-{}.ferrodesign", ferrocast_store::new_id()));
    let manifest = app.engine.export_design(&id, &path).await?;
    let bytes = tokio::fs::read(&path).await;
    let _ = tokio::fs::remove_file(&path).await;
    let bytes = bytes.map_err(|e| ApiError::from(ferrocast_store::StoreError::Io(e)))?;
    let stem: String = manifest
        .name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    let stem = if stem.is_empty() { "design".to_string() } else { stem };
    let mut res = axum::body::Body::from(bytes).into_response();
    let h = res.headers_mut();
    h.insert(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static("application/zip"));
    if let Ok(v) = axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{stem}.ferrodesign\"")) {
        h.insert(axum::http::header::CONTENT_DISPOSITION, v);
    }
    Ok(res)
}

fn too_large() -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "too_large",
        format!("Design files can be up to {} MB.", ferrocast_store::designfile::MAX_FILE / 1024 / 1024),
    )
    .hint("Remove large videos from the design, then export it again.")
}

fn upload_error(e: axum::extract::multipart::MultipartError) -> ApiError {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        too_large()
    } else {
        ApiError::invalid(format!("The upload didn't arrive whole: {}", e.body_text()))
            .hint("Try again; if it keeps failing, get the file again.")
    }
}

/// `POST /designs/import`: multipart `file` (a `.ferrodesign`) → new designs of your own.
pub async fn import(
    State(app): State<AppState>,
    form: Result<Multipart, MultipartRejection>,
) -> ApiResult<impl IntoResponse> {
    use tokio::io::AsyncWriteExt;
    let mut form = form.map_err(|_| {
        ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "wrong_content_type",
            "Send the design file as multipart/form-data in a field named \"file\".",
        )
    })?;
    let mut field = loop {
        match form.next_field().await {
            Ok(Some(f)) if f.name() == Some("file") => break f,
            Ok(Some(_)) => continue,
            Ok(None) => return Err(ApiError::invalid("Attach the .ferrodesign in a form field named \"file\".")),
            Err(e) => return Err(upload_error(e)),
        }
    };
    let io = |e: std::io::Error| ApiError::from(ferrocast_store::StoreError::Io(e));
    let dir = app.engine.store().dir().join("tmp");
    tokio::fs::create_dir_all(&dir).await.map_err(io)?;
    let path = dir.join(format!("import-{}.ferrodesign", ferrocast_store::new_id()));
    let mut out = tokio::fs::File::create(&path).await.map_err(io)?;
    let mut size: u64 = 0;
    let copied: ApiResult<()> = async {
        while let Some(chunk) = field.chunk().await.map_err(upload_error)? {
            size += chunk.len() as u64;
            if size > ferrocast_store::designfile::MAX_FILE {
                return Err(too_large());
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
        return Err(ApiError::invalid("The file is empty.").hint("Pick a .ferrodesign exported from Ferrocast."));
    }
    let result = app.engine.import_design_file(&path).await;
    let _ = tokio::fs::remove_file(&path).await;
    Ok((StatusCode::CREATED, Json(json!(result?))))
}
