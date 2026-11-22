//! Stateful `.overlay` theme-pack preview, apply (copy media + resolve Google Fonts + create
//! native Ferrocast overlays + add & switch Studio collection), and one-click undo on `Engine`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use ferrocast_core::overlay::{Overlay, WidgetKind};
use ferrocast_store::repo::overlays;
use ferrocast_store::{durable, new_id, new_token, studio_library};
use serde::{Deserialize, Serialize};

use crate::fonts::{
    DEFAULT_GOOGLE_FONTS_API, FontPreviewEntry, FontResultEntry, preview_fonts, resolve_and_install_fonts,
};
use crate::import::PREVIEW_TTL;
use crate::import::overlay_parse::{
    OverlayCanvas, OverlayPackFormat, ParsedPack, PreviewScene, PreviewWidget, UnknownSource, finalize_doc,
    parse_overlay_zip,
};
use crate::studio_host::ShelfAct;
use crate::{Engine, EngineError, now};

/// Parsed packs held in memory between a preview and its import (each can be 300 MB).
const MAX_STAGED_PREVIEWS: usize = 2;

#[derive(Debug, Clone)]
pub(crate) struct StagedOverlay {
    id: String,
    parsed: ParsedPack,
    expires_at: Instant,
}

pub(crate) type OverlayPreviewSlot = Mutex<Vec<StagedOverlay>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayPackPreview {
    pub preview_id: String,
    pub suggested_name: String,
    pub format: OverlayPackFormat,
    pub canvas: OverlayCanvas,
    pub scene_count: usize,
    pub source_count: usize,
    pub scenes: Vec<PreviewScene>,
    pub widgets: Vec<PreviewWidget>,
    pub media_files: usize,
    pub media_bytes: u64,
    pub missing_assets: Vec<String>,
    pub unknown_sources: Vec<UnknownSource>,
    pub fonts: Vec<FontPreviewEntry>,
    pub notes: Vec<String>,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OverlayPackImportReq {
    pub preview_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub create_overlays: Option<bool>,
    #[serde(default)]
    pub fetch_fonts: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatedOverlaySummary {
    pub id: String,
    pub name: String,
    pub scene: String,
    pub url: String,
    pub widget_kinds: Vec<WidgetKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayPackImportReport {
    pub id: String,
    pub name: String,
    pub format: OverlayPackFormat,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_id: Option<String>,
    pub undo_token: String,
    pub scenes: usize,
    pub sources: usize,
    pub web_pages: usize,
    pub widgets_mapped: usize,
    pub overlays_created: Vec<CreatedOverlaySummary>,
    pub media_copied: usize,
    pub media_bytes: u64,
    pub missing_assets: Vec<String>,
    pub unknown_sources: Vec<UnknownSource>,
    pub missing: Vec<String>,
    pub fonts: Vec<FontResultEntry>,
    pub offline_font_fallback: bool,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OverlayPackUndoReq {
    pub undo_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayPackUndoReport {
    pub removed_collection_id: String,
    pub removed_collection_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restored_collection_id: Option<String>,
    pub removed_overlays: usize,
    pub deleted_files: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UndoRecord {
    token: String,
    collection_id: String,
    collection_name: String,
    previous_id: Option<String>,
    pack_id: String,
    overlay_ids: Vec<String>,
}

fn packs_root(data: &Path) -> PathBuf {
    data.join("studio").join("packs")
}

fn undo_dir(data: &Path) -> PathBuf {
    data.join("studio").join("overlay-undo")
}

fn undo_file(data: &Path, token: &str) -> Result<PathBuf, EngineError> {
    if token.is_empty()
        || token.len() > 80
        || !token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(EngineError::Invalid("undo_token: not a valid undo token.".into()));
    }
    Ok(undo_dir(data).join(format!("{token}.json")))
}

fn stage_preview(slot: &OverlayPreviewSlot, id: String, parsed: ParsedPack) -> i64 {
    let mut guard = slot.lock().unwrap_or_else(|p| p.into_inner());
    let now_inst = Instant::now();
    guard.retain(|s| s.expires_at > now_inst);
    while guard.len() >= MAX_STAGED_PREVIEWS {
        guard.remove(0);
    }
    guard.push(StagedOverlay { id, parsed, expires_at: now_inst + PREVIEW_TTL });
    now().0 + (PREVIEW_TTL.as_millis() as i64)
}

/// The suggested name of a staged preview, without taking it (so a bad name can be corrected).
fn peek_suggested_name(slot: &OverlayPreviewSlot, preview_id: &str) -> Result<String, EngineError> {
    let guard = slot.lock().unwrap_or_else(|p| p.into_inner());
    let now_inst = Instant::now();
    guard
        .iter()
        .find(|s| s.id == preview_id && s.expires_at > now_inst)
        .map(|s| s.parsed.suggested_name.clone())
        .ok_or_else(no_preview)
}

fn no_preview() -> EngineError {
    EngineError::Refused {
        status: 404,
        code: "no_preview",
        message: "That theme pack preview has expired or already been imported.".into(),
        hint: "Pick the .overlay file again to preview it.".into(),
        retry_after: None,
    }
}

fn take_staged_preview(slot: &OverlayPreviewSlot, preview_id: &str) -> Result<StagedOverlay, EngineError> {
    let mut guard = slot.lock().unwrap_or_else(|p| p.into_inner());
    let now_inst = Instant::now();
    guard.retain(|s| s.expires_at > now_inst);
    let pos = guard.iter().position(|s| s.id == preview_id).ok_or_else(no_preview)?;
    Ok(guard.remove(pos))
}

fn sanitize_pack_dest(pack_dir: &Path, rel_path: &str) -> Option<PathBuf> {
    let p = pack_dir.join(rel_path);
    if p.starts_with(pack_dir) { Some(p) } else { None }
}

/// Writes a pack's files under `pack_dir`; on any failure what was written is removed, so a
/// failed import leaves no half-made folder behind. Returns each file's path, and the count and
/// size of the media.
fn copy_pack_assets(
    pack_dir: &Path,
    assets: Vec<super::overlay_parse::UnpackedAsset>,
) -> Result<(Vec<String>, usize, u64), EngineError> {
    let copy = || -> Result<(Vec<String>, usize, u64), EngineError> {
        let mut out_paths = Vec::with_capacity(assets.len());
        let mut count = 0usize;
        let mut bytes = 0u64;
        for asset in &assets {
            let Some(dest) = sanitize_pack_dest(pack_dir, &asset.rel_path) else {
                return Err(EngineError::Internal("Rejected unsafe destination path.".into()));
            };
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| EngineError::Internal(format!("Couldn't prepare pack folder: {e}")))?;
            }
            std::fs::write(&dest, &asset.bytes)
                .map_err(|e| EngineError::Internal(format!("Couldn't copy media \"{}\": {e}", asset.rel_path)))?;
            if asset.kind.is_media() {
                count += 1;
                bytes += asset.bytes.len() as u64;
            }
            out_paths.push(dest.display().to_string());
        }
        Ok((out_paths, count, bytes))
    };
    let res = copy();
    if res.is_err() {
        let _ = count_and_remove_dir(pack_dir);
    }
    res
}

/// Removes `dir` and says how many files were in it. Links are counted and removed as links:
/// what they point at is never walked into or deleted.
fn count_and_remove_dir(dir: &Path) -> usize {
    fn walk_count(p: &Path) -> usize {
        let Ok(rd) = std::fs::read_dir(p) else { return 0 };
        let mut n = 0;
        for entry in rd.flatten() {
            // `file_type` doesn't follow a link, so a link to a folder is one file, not a folder.
            match entry.file_type() {
                Ok(t) if t.is_dir() => n += walk_count(&entry.path()),
                _ => n += 1,
            }
        }
        n
    }
    if !std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
        return 0;
    }
    let files = walk_count(dir);
    let _ = std::fs::remove_dir_all(dir);
    files
}

impl Engine {
    /// Overrides the Google Fonts API origin for tests (defaults to `https://fonts.googleapis.com`).
    pub fn set_google_fonts_api(&self, base: &str) {
        *self.inner.google_fonts_api.write().unwrap_or_else(|p| p.into_inner()) =
            base.trim_end_matches('/').to_string();
    }

    pub fn google_fonts_api(&self) -> String {
        self.inner.google_fonts_api.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Parses and audits an uploaded `.overlay` zip and returns a non-destructive preview.
    pub async fn preview_overlay_pack(
        &self,
        zip_bytes: Vec<u8>,
        filename: String,
    ) -> Result<OverlayPackPreview, EngineError> {
        // One pack is read at a time: previews don't queue up, each holding a lot of memory.
        let _one_at_a_time = self.inner.import_busy.try_lock().map_err(|_| super::busy())?;
        let data = self.store().dir().to_path_buf();
        let parsed = tokio::task::spawn_blocking(move || parse_overlay_zip(&zip_bytes, &filename))
            .await
            .map_err(|e| EngineError::Internal(e.to_string()))??;

        let data_for_fonts = data.clone();
        let font_specs = parsed.font_specs.clone();
        let bundled = parsed.bundled_fonts.clone();
        let fonts = tokio::task::spawn_blocking(move || {
            let installed = ferrocore::fonts::installed_system_font_families();
            preview_fonts(&data_for_fonts, &font_specs, &bundled, &installed)
        })
        .await
        .map_err(|e| EngineError::Internal(e.to_string()))?;

        let media_files = parsed.assets.iter().filter(|a| a.kind.is_media()).count();
        let media_bytes: u64 = parsed.assets.iter().filter(|a| a.kind.is_media()).map(|a| a.bytes.len() as u64).sum();

        let preview_id = new_id();
        let out = OverlayPackPreview {
            preview_id: preview_id.clone(),
            suggested_name: parsed.suggested_name.clone(),
            format: parsed.format,
            canvas: parsed.canvas,
            scene_count: parsed.scenes.len(),
            source_count: parsed.source_count,
            scenes: parsed.scenes.clone(),
            widgets: parsed.widgets.clone(),
            media_files,
            media_bytes,
            missing_assets: parsed.missing_assets.clone(),
            unknown_sources: parsed.unknown_sources.clone(),
            fonts,
            notes: parsed.notes.clone(),
            expires_at: 0,
        };
        let expires_at = stage_preview(&self.inner.overlay_import, preview_id, parsed);
        Ok(OverlayPackPreview { expires_at, ..out })
    }

    /// Imports a previously previewed `.overlay` theme pack: resolves & caches missing Google Fonts,
    /// copies media files into `<data>/studio/packs/<pack_id>/`, creates Ferrocast overlays for mapped
    /// widgets, and switches the active Studio scene collection.
    pub async fn import_overlay_pack(&self, req: OverlayPackImportReq) -> Result<OverlayPackImportReport, EngineError> {
        if req.preview_id.trim().is_empty() {
            return Err(EngineError::Invalid("preview_id: preview the .overlay file first.".into()));
        }
        let _one_at_a_time = self.inner.import_busy.lock().await;

        // Refuse before copying files or switching when live streaming or recording is active.
        self.ensure_idle_for_switch("scene collections").await?;

        // The name is checked before the preview is taken: a typo mustn't cost a 150 MB re-upload.
        let preview_id = req.preview_id.trim();
        let suggested = peek_suggested_name(&self.inner.overlay_import, preview_id)?;
        let clean_name =
            studio_library::check_name(req.name.as_deref().unwrap_or(&suggested)).map_err(EngineError::Invalid)?;
        let staged = take_staged_preview(&self.inner.overlay_import, preview_id)?;
        let mut parsed = staged.parsed;

        let data = self.store().dir().to_path_buf();
        let pack_id = new_id();
        let pack_dir = packs_root(&data).join(&pack_id);

        // 1. Copy validated media assets into Ferrocast's own packs directory.
        // Moved, not copied: a pack's media can be hundreds of megabytes.
        let assets = std::mem::take(&mut parsed.assets);
        let pack_dir_for_copy = pack_dir.clone();
        let (asset_paths, media_copied, media_bytes) =
            tokio::task::spawn_blocking(move || copy_pack_assets(&pack_dir_for_copy, assets))
                .await
                .map_err(|e| EngineError::Internal(e.to_string()))??;

        // 2. Resolve & install missing fonts solely from Google Fonts into `<data>/fonts/`.
        let installed = tokio::task::spawn_blocking(ferrocore::fonts::installed_system_font_families)
            .await
            .map_err(|e| EngineError::Internal(e.to_string()))?;
        let google_base = self.google_fonts_api();
        let fetch_fonts = req.fetch_fonts.unwrap_or(true);
        let (font_results, offline_font_fallback) = resolve_and_install_fonts(
            &data,
            &pack_id,
            &parsed.font_specs,
            &parsed.bundled_fonts,
            &installed,
            &google_base,
            fetch_fonts,
        )
        .await;

        // 3. Create native Ferrocast Overlays for mapped widgets/streamlabels where requested.
        let mut created_overlays: Vec<Overlay> = Vec::new();
        let mut overlay_urls: BTreeMap<String, String> = BTreeMap::new();
        let mut overlays_summary: Vec<CreatedOverlaySummary> = Vec::new();

        if req.create_overlays.unwrap_or(true) && !parsed.scene_overlays.is_empty() {
            for planned in &parsed.scene_overlays {
                let title = format!("{} · {}", clean_name, planned.scene_name).chars().take(60).collect::<String>();
                let draft = Overlay {
                    id: String::new(),
                    name: title,
                    token: String::new(),
                    width: parsed.canvas.width as i32,
                    height: parsed.canvas.height as i32,
                    widgets: planned.widgets.clone(),
                    updated_at: 0,
                    design: None,
                };
                match self.db(move |s| overlays::create_from(s, draft)).await {
                    Ok(saved) => {
                        let view = self.overlay_view(&saved);
                        let kinds: Vec<WidgetKind> = saved.widgets.iter().map(|w| w.kind).collect();
                        overlay_urls.insert(planned.placeholder_key.clone(), view.url.clone());
                        overlays_summary.push(CreatedOverlaySummary {
                            id: saved.id.clone(),
                            name: saved.name.clone(),
                            scene: planned.scene_name.clone(),
                            url: view.url,
                            widget_kinds: kinds,
                        });
                        created_overlays.push(saved);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, scene = %planned.scene_name, "skipped creating Ferrocast overlay for pack scene");
                    }
                }
            }
        }

        // 4. Finalize collection document and import it into Studio library (Desktop live host or disk).
        let final_doc = finalize_doc(&parsed.doc_template, &asset_paths, &overlay_urls);
        let rollback_media_and_overlays = || async {
            let _ = count_and_remove_dir(&pack_dir);
            let _ = count_and_remove_dir(&crate::fonts::pack_fonts_dir(&data, &pack_id));
            for ov in &created_overlays {
                let id = ov.id.clone();
                let _ = self.db(move |s| overlays::delete(s, &id)).await;
            }
        };

        // The checks at the start were a while ago (media copied, fonts downloaded): a show or a
        // recording may have started since, and the collection mustn't be swapped under it.
        if let Err(e) = self.ensure_idle_for_switch("scene collections").await {
            rollback_media_and_overlays().await;
            return Err(e);
        }

        let host_opt = self.inner.studio_host.optional();
        let (col_id, saved_name, previous_id) = if let Some(host) = host_opt {
            let data_prev = data.clone();
            let prev =
                tokio::task::spawn_blocking(move || studio_library::load(&data_prev).collections.current).await.ok();
            let res =
                host.shelf("collections".into(), ShelfAct::Import { name: clean_name.clone(), doc: final_doc }).await;
            match res {
                Ok(v) => (
                    v["id"].as_str().unwrap_or("").to_string(),
                    v["name"].as_str().unwrap_or(&clean_name).to_string(),
                    prev,
                ),
                Err(e) => {
                    rollback_media_and_overlays().await;
                    return Err(crate::studio_host::failed(e));
                }
            }
        } else {
            let data_for_lib = data.clone();
            let name_clone = clean_name.clone();
            let res = tokio::task::spawn_blocking(move || {
                studio_library::import_collection_doc(&data_for_lib, &name_clone, final_doc)
            })
            .await
            .map_err(|e| EngineError::Internal(e.to_string()))?;
            match res {
                Ok((id, prev, sname)) => (id, sname, Some(prev)),
                Err(msg) => {
                    rollback_media_and_overlays().await;
                    return Err(crate::studio_host::failed(msg));
                }
            }
        };

        // 5. Persist one-click Undo record cleanly on disk.
        let undo_token = new_token();
        let undo_rec = UndoRecord {
            token: undo_token.clone(),
            collection_id: col_id.clone(),
            collection_name: saved_name.clone(),
            previous_id: previous_id.clone(),
            pack_id,
            overlay_ids: created_overlays.iter().map(|o| o.id.clone()).collect(),
        };
        if let Ok(path) = undo_file(&data, &undo_token) {
            if let Some(p) = path.parent() {
                let _ = std::fs::create_dir_all(p);
            }
            if let Err(e) = durable::save_json_pretty(&path, &undo_rec) {
                tracing::warn!(error = %e, "couldn't write overlay undo record");
            }
        }

        let mut notes = parsed.notes;
        if offline_font_fallback {
            notes.push(
                "Some fonts couldn't be downloaded because Google Fonts couldn't be reached. Fallback system fonts will be used until you import while online."
                    .into(),
            );
        }
        if google_base == DEFAULT_GOOGLE_FONTS_API {
            notes.push("Downloaded fonts are stored inside Ferrocast's own font folder only.".into());
        }

        let widgets_mapped = parsed.widgets.iter().filter(|w| w.supported).count();

        Ok(OverlayPackImportReport {
            id: col_id,
            name: saved_name,
            format: parsed.format,
            previous_id,
            undo_token,
            scenes: parsed.scenes.len(),
            sources: parsed.source_count,
            web_pages: parsed.web_page_count,
            widgets_mapped,
            overlays_created: overlays_summary,
            media_copied,
            media_bytes,
            missing_assets: parsed.missing_assets,
            unknown_sources: parsed.unknown_sources,
            missing: parsed.missing_kinds,
            fonts: font_results,
            offline_font_fallback,
            notes,
        })
    }

    /// Reverts a previous `.overlay` pack import: restores the previous scene collection, removes the
    /// imported collection, deletes any Ferrocast overlays created by the pack, and cleans up copied
    /// pack media files cleanly (cached Google Fonts remain reusable in `<data>/fonts/`).
    pub async fn undo_overlay_pack(&self, undo_token: &str) -> Result<OverlayPackUndoReport, EngineError> {
        let _one_at_a_time = self.inner.import_busy.lock().await;
        self.ensure_idle_for_switch("scene collections").await?;

        let data = self.store().dir().to_path_buf();
        let rec_path = undo_file(&data, undo_token.trim())?;
        let rec: UndoRecord =
            durable::load_json_with_backup(&rec_path, |_| true).map(|l| l.value).ok_or_else(|| {
                EngineError::Refused {
                    status: 404,
                    code: "no_undo",
                    message: "Nothing to undo for that import anymore.".into(),
                    hint: "The import may already have been undone or removed.".into(),
                    retry_after: None,
                }
            })?;

        let host_opt = self.inner.studio_host.optional();
        let restored_id = if let Some(host) = host_opt {
            // Ensure we switch off the imported collection first before removing it.
            let target_restore = if let Some(prev) = &rec.previous_id {
                let data_c = data.clone();
                let prev_c = prev.clone();
                let exists = tokio::task::spawn_blocking(move || {
                    studio_library::load(&data_c).collections.get(&prev_c).is_some()
                })
                .await
                .unwrap_or(false);
                if exists { Some(prev.clone()) } else { None }
            } else {
                None
            };
            if let Some(prev_id) = &target_restore {
                let _ = host.shelf("collections".into(), ShelfAct::Switch { id: prev_id.clone() }).await;
            }
            host.shelf("collections".into(), ShelfAct::Remove { id: rec.collection_id.clone() })
                .await
                .map_err(crate::studio_host::failed)?;
            target_restore
        } else {
            let data_for_rb = data.clone();
            let remove_id = rec.collection_id.clone();
            let restore_id = rec.previous_id.clone().unwrap_or_default();
            let restored = tokio::task::spawn_blocking(move || {
                studio_library::rollback_imported_collection(&data_for_rb, &remove_id, &restore_id)
            })
            .await
            .map_err(|e| EngineError::Internal(e.to_string()))?
            .map_err(crate::studio_host::failed)?;
            Some(restored)
        };

        let mut removed_overlays = 0usize;
        for oid in &rec.overlay_ids {
            let id = oid.clone();
            if self.db(move |s| overlays::delete(s, &id)).await.is_ok() {
                removed_overlays += 1;
            }
        }

        let packs_base = packs_root(&data);
        let pack_ok = !rec.pack_id.is_empty()
            && rec.pack_id.len() <= 64
            && rec.pack_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        let pack_dir = packs_base.join(&rec.pack_id);
        let deleted_files = if pack_ok && pack_dir != packs_base && pack_dir.starts_with(&packs_base) {
            tokio::task::spawn_blocking(move || count_and_remove_dir(&pack_dir)).await.unwrap_or(0)
        } else {
            0
        };
        // The pack's own fonts go with it (the shared cache of Google fonts stays).
        if pack_ok {
            let fonts_of_pack = crate::fonts::pack_fonts_dir(&data, &rec.pack_id);
            let _ = tokio::task::spawn_blocking(move || count_and_remove_dir(&fonts_of_pack)).await;
        }
        durable::remove_with_backup(&rec_path);

        Ok(OverlayPackUndoReport {
            removed_collection_id: rec.collection_id,
            removed_collection_name: rec.collection_name,
            restored_collection_id: restored_id,
            removed_overlays,
            deleted_files,
        })
    }
}

#[cfg(test)]
#[path = "overlay_tests.rs"]
mod tests;
