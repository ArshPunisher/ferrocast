//! Designs (API.md §4 Designs): the built-in catalogue that ships with the app, the
//! streamer's own designs, the alert design every alert plays with, "Use" (turning a theme
//! or scene into overlays) and `.ferrodesign` files.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use ferrocast_core::design::{Design, DesignContent, DesignKind, Origin, OriginKind, is_brand_layer};
use ferrocast_core::overlay::{DesignRef, MAX_WIDGETS, Overlay, Widget};
use ferrocast_store::designfile::{self, DesignFileError};
use ferrocast_store::{StoreError, repo};
use serde::{Deserialize, Serialize};

use crate::snapshot::OverlayView;
use crate::{Engine, EngineError, now};

/// Built-in ids all start with this; the streamer's own never do.
pub const BUILTIN_PREFIX: &str = "builtin.";

/// What "Use" should do (API.md `POST /designs/{id}/use`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UseRequest {
    /// Put the scene on this overlay (its URL stays the same) instead of making a new one.
    /// Required for widget designs.
    pub overlay_id: Option<String>,
    /// Theme scenes to use (keys); all of them when empty.
    pub scenes: Vec<String>,
    /// Themes: also play alerts with the theme's alert design (default true).
    pub alert_style: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UseResult {
    /// The alert design in use afterwards.
    pub alert_design: Option<String>,
    /// Overlays created or changed.
    pub overlays: Vec<OverlayView>,
}

fn built_in_refusal(action: &str) -> EngineError {
    EngineError::Refused {
        status: 409,
        code: "built_in",
        message: format!("Built-in designs can't be {action}."),
        hint: "Use Duplicate & customise to make your own copy, then change that.".into(),
        retry_after: None,
    }
}

fn store_err(e: StoreError) -> EngineError {
    match e {
        // The store's checks say what's wrong with a field: that's a 400 for the API.
        StoreError::Conflict(m) if m.contains(':') && !m.contains("can't become") => EngineError::Invalid(m),
        other => EngineError::from(other),
    }
}

/// A design-file problem as an API error with its own status.
pub fn design_file_error(e: DesignFileError) -> EngineError {
    let message = e.to_string();
    let (status, code, hint) = match e {
        DesignFileError::NotADesignFile => {
            (415, "bad_design_file", "Pick a .ferrodesign file exported from Ferrocast.")
        }
        DesignFileError::Unsafe(_) => {
            (415, "unsafe_design_file", "Ask whoever shared it to export it again from Ferrocast.")
        }
        DesignFileError::Damaged(_) => {
            (415, "damaged_design_file", "The file may not have finished downloading. Get it again.")
        }
        DesignFileError::MediaType(_) => {
            (415, "unsupported_type", "Replace that file in the design, then export again.")
        }
        DesignFileError::TooNew(_) => (422, "design_too_new", "Update Ferrocast, then import it again."),
        DesignFileError::Invalid(_) => {
            (422, "invalid_design", "Ask whoever shared it to open and save it in Ferrocast, then export again.")
        }
        DesignFileError::TooLarge(_) => {
            (413, "too_large", "Remove large videos from the design, then export it again.")
        }
    };
    EngineError::Refused { status, code, message, hint: hint.into(), retry_after: None }
}

fn short(name: &str, max: usize) -> String {
    name.trim().chars().take(max).collect::<String>().trim_end().to_string()
}

impl Engine {
    /// Loads the catalogue that ships with the app (`designs/catalog.json` in the overlay
    /// bundle). Anything that isn't a valid built-in is left out with a warning.
    pub async fn set_builtin_designs(&self, list: Vec<Design>) {
        let mut ok = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for mut d in list {
            d.built_in = true;
            if !d.id.starts_with(BUILTIN_PREFIX) || !seen.insert(d.id.clone()) {
                tracing::warn!(id = %d.id, "skipped a built-in design with a bad or repeated id");
                continue;
            }
            match d.validate() {
                Ok(()) => ok.push(d),
                Err(e) => tracing::warn!(id = %d.id, error = %e, "skipped a broken built-in design"),
            }
        }
        *self.inner.builtins.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(ok);
        self.refresh_active_design().await;
    }

    pub fn builtin_designs(&self) -> Arc<Vec<Design>> {
        self.inner.builtins.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn builtin(&self, id: &str) -> Option<Design> {
        self.builtin_designs().iter().find(|d| d.id == id).cloned()
    }

    /// Built-ins first (catalogue order), then the streamer's own, newest first.
    pub async fn designs(&self, kind: Option<DesignKind>) -> Result<Vec<Design>, EngineError> {
        let mine = self.db(repo::designs::list).await?;
        Ok(self.builtin_designs().iter().cloned().chain(mine).filter(|d| kind.is_none_or(|k| d.kind() == k)).collect())
    }

    pub async fn design(&self, id: &str) -> Result<Design, EngineError> {
        if let Some(d) = self.builtin(id) {
            return Ok(d);
        }
        if id.starts_with(BUILTIN_PREFIX) {
            return Err(EngineError::NotFound("design"));
        }
        let did = id.to_string();
        self.db(move |s| repo::designs::get(s, &did)).await.map_err(|e| match e {
            EngineError::Store(StoreError::NotFound(what)) => EngineError::NotFound(what),
            other => other,
        })
    }

    /// Saves a new design as the streamer's own: fresh id, never built-in or branded.
    pub async fn create_design(&self, d: Design) -> Result<Design, EngineError> {
        let kind = match d.origin.kind {
            OriginKind::Duplicate | OriginKind::AssetPack | OriginKind::Scratch => d.origin.kind,
            OriginKind::BuiltIn | OriginKind::Import => OriginKind::Scratch,
        };
        let from = d.origin.from.as_deref().map(|f| short(f, 120));
        let mine = d.into_user(ferrocast_store::new_id(), Origin { kind, from }, now().0);
        let saved = mine.clone();
        self.db(move |s| repo::designs::insert(s, &saved)).await.map_err(|e| match e {
            EngineError::Store(s) => store_err(s),
            other => other,
        })
    }

    pub async fn update_design(&self, id: &str, mut d: Design) -> Result<Design, EngineError> {
        if id.starts_with(BUILTIN_PREFIX) {
            return Err(built_in_refusal("changed"));
        }
        d.id = id.to_string();
        d.pro = false;
        if d.built_in || d.branded {
            return Err(EngineError::Invalid("branded: only built-in designs carry the Ferrocast mark.".into()));
        }
        let saved = self.db(move |s| repo::designs::update(s, &d)).await.map_err(|e| match e {
            EngineError::Store(s) => store_err(s),
            other => other,
        })?;
        self.refresh_active_design().await;
        self.refresh_following(&saved).await;
        Ok(saved)
    }

    /// Overlays made from `design` that still follow it get its new layout (their URLs stay).
    async fn refresh_following(&self, design: &Design) {
        let scenes: Vec<&ferrocast_core::design::Scene> = match &design.content {
            DesignContent::Theme(t) => t.scenes.iter().collect(),
            DesignContent::Scene(s) => vec![s],
            _ => return,
        };
        let Ok(list) = self.db(repo::overlays::list).await else { return };
        for o in list {
            let Some(link) = o.design.as_ref().filter(|d| d.follows && d.id == design.id) else { continue };
            let Some(scene) = scenes.iter().find(|s| s.key == link.scene) else { continue };
            let _one_at_a_time = self.config_lock().await;
            if let Err(e) = self.place_scene(design, scene, &o.name, Some(&o.id)).await {
                tracing::warn!(error = %e, overlay = %o.id, "an overlay couldn't follow its design");
            }
        }
    }

    /// "Follow the theme again": the overlay takes its design's layout and follows it.
    pub async fn follow_design(&self, overlay_id: &str) -> Result<OverlayView, EngineError> {
        let o = self.overlay(overlay_id).await?;
        let Some(link) = o.design.clone() else {
            return Err(EngineError::Invalid("design: this overlay wasn't made from a design.".into()));
        };
        let design = self.design(&link.id).await?;
        let scene = match &design.content {
            DesignContent::Theme(t) => t.scenes.iter().find(|s| s.key == link.scene).cloned(),
            DesignContent::Scene(s) => Some(s.clone()),
            _ => None,
        }
        .ok_or_else(|| EngineError::Invalid("design: that screen isn't in the design any more.".into()))?;
        let _one_at_a_time = self.config_lock().await;
        self.place_scene(&design, &scene, &o.name, Some(&o.id)).await
    }

    pub async fn delete_design(&self, id: &str) -> Result<(), EngineError> {
        if id.starts_with(BUILTIN_PREFIX) {
            return Err(built_in_refusal("deleted"));
        }
        if self.active_alert_design().await?.as_deref() == Some(id) {
            return Err(EngineError::Refused {
                status: 409,
                code: "in_use",
                message: "Your alerts play with this design.".into(),
                hint: "Pick another alert design in the gallery first, then delete this one.".into(),
                retry_after: None,
            });
        }
        let did = id.to_string();
        self.db(move |s| repo::designs::delete(s, &did)).await
    }

    /// The streamer's own copy of any design, without branding. A theme's built-in alert
    /// design is copied along, so using the copy never brings the mark back.
    pub async fn duplicate_design(&self, id: &str, name: Option<&str>) -> Result<Design, EngineError> {
        let source = self.design(id).await?;
        let name = name.map(|n| short(n, ferrocast_core::design::MAX_NAME)).filter(|n| !n.is_empty());
        let mut copy = source.clone();
        copy.name = name.unwrap_or_else(|| short(&format!("{} copy", source.name), ferrocast_core::design::MAX_NAME));
        if let DesignContent::Theme(t) = &mut copy.content
            && let Some(alert_id) = t.alert_design.clone()
            && alert_id.starts_with(BUILTIN_PREFIX)
        {
            t.alert_design = match self.builtin(&alert_id) {
                Some(a) => Some(Box::pin(self.duplicate_design(&a.id, None)).await?.id),
                None => None,
            };
        }
        let origin = Origin { kind: OriginKind::Duplicate, from: Some(short(&source.name, 120)) };
        let mine = copy.into_user(ferrocast_store::new_id(), origin, now().0);
        let saved = mine.clone();
        self.db(move |s| repo::designs::insert(s, &saved)).await.map_err(|e| match e {
            EngineError::Store(s) => store_err(s),
            other => other,
        })
    }

    pub async fn active_alert_design(&self) -> Result<Option<String>, EngineError> {
        self.db(repo::designs::active_alert).await
    }

    /// Picks the alert design every alert plays with (`None`: the rules' own look).
    pub async fn set_active_alert_design(&self, id: Option<&str>) -> Result<Option<String>, EngineError> {
        if let Some(id) = id {
            let d = self.design(id).await?;
            if d.kind() != DesignKind::Alert {
                return Err(EngineError::Invalid(format!(
                    "design_id: \"{}\" is a {} design; pick an alert design.",
                    short(&d.name, 40),
                    d.kind().key()
                )));
            }
        }
        let value = id.map(str::to_string);
        let v = value.clone();
        self.db(move |s| repo::designs::set_active_alert(s, v.as_deref())).await?;
        self.refresh_active_design().await;
        Ok(value)
    }

    /// Re-reads the alert design in use into memory (the pipeline looks at it per alert).
    pub(crate) async fn refresh_active_design(&self) {
        let id = self.active_alert_design().await.ok().flatten();
        let design = match id {
            Some(id) => self.design(&id).await.ok().filter(|d| d.kind() == DesignKind::Alert),
            None => None,
        };
        self.state_mut().active_design = design;
    }

    /// The design a test alert should use instead of the active one, once.
    pub(crate) fn take_test_design(&self, event: &ferrocast_core::event::Event) -> Option<Design> {
        let key = event.source_id.as_deref()?.split('#').next()?;
        if !key.starts_with("design-test-") || event.origin != ferrocast_core::event::Origin::Test {
            return None;
        }
        self.inner.test_designs.lock().unwrap_or_else(|p| p.into_inner()).remove(key)
    }

    pub(crate) fn put_test_design(&self, key: &str, d: Design) {
        let mut map = self.inner.test_designs.lock().unwrap_or_else(|p| p.into_inner());
        if map.len() > 50 {
            map.clear();
        }
        map.insert(key.to_string(), d);
    }

    /// "Use": an alert design starts playing alerts; a theme or scene becomes overlays (or
    /// replaces the layout of the one picked, keeping its URL); a widget design adds that
    /// widget to the overlay picked.
    pub async fn use_design(&self, id: &str, req: UseRequest) -> Result<UseResult, EngineError> {
        let design = self.design(id).await?;
        let _one_at_a_time = self.config_lock().await;
        let mut changed = Vec::new();
        let mut alert_design = self.active_alert_design().await?;
        match &design.content {
            DesignContent::Alert(_) => {
                drop(_one_at_a_time);
                alert_design = self.set_active_alert_design(Some(&design.id)).await?;
                return Ok(UseResult { alert_design, overlays: changed });
            }
            DesignContent::Theme(t) => {
                let scenes: Vec<_> = if req.scenes.is_empty() {
                    t.scenes.iter().collect()
                } else {
                    let picked: Vec<_> = t.scenes.iter().filter(|s| req.scenes.contains(&s.key)).collect();
                    if picked.len() != req.scenes.len() {
                        return Err(EngineError::Invalid("scenes: one of those scenes isn't in this theme.".into()));
                    }
                    picked
                };
                if req.overlay_id.is_some() && scenes.len() != 1 {
                    return Err(EngineError::Invalid(
                        "overlay_id: pick one scene to put on an existing overlay.".into(),
                    ));
                }
                for scene in scenes {
                    let name = short(&format!("{} ({})", scene.name, design.name), 60);
                    changed.push(self.place_scene(&design, scene, &name, req.overlay_id.as_deref()).await?);
                }
                if req.alert_style.unwrap_or(true)
                    && let Some(a) = &t.alert_design
                    && self.design(a).await.is_ok_and(|d| d.kind() == DesignKind::Alert)
                {
                    let a = a.clone();
                    drop(_one_at_a_time);
                    alert_design = self.set_active_alert_design(Some(&a)).await?;
                }
            }
            DesignContent::Scene(scene) => {
                let name = short(&design.name, 60);
                changed.push(self.place_scene(&design, scene, &name, req.overlay_id.as_deref()).await?);
            }
            DesignContent::Widget(w) => {
                let Some(oid) = req.overlay_id.as_deref() else {
                    return Err(EngineError::Invalid("overlay_id: pick the overlay to add this widget to.".into()));
                };
                let mut o = self.overlay(oid).await?;
                if o.widgets.len() >= MAX_WIDGETS {
                    return Err(EngineError::Conflict(format!(
                        "That overlay already has {MAX_WIDGETS} widgets, the most one can hold. Remove one first."
                    )));
                }
                let (ww, hh) = (w.w.min(o.width), w.h.min(o.height));
                let n = (1..).find(|i| !o.widgets.iter().any(|x| x.id == format!("{}-{i}", w.widget_key())));
                let wid = format!("{}-{}", w.widget_key(), n.unwrap_or(1));
                o.widgets.push(Widget::new(
                    &wid,
                    w.widget,
                    ((o.width - ww) / 2, (o.height - hh) / 2, ww, hh),
                    w.config_with_style(&design.id, design.branded),
                ));
                changed.push(self.update_overlay(o).await?);
            }
        }
        Ok(UseResult { alert_design, overlays: changed })
    }

    pub(crate) async fn place_scene(
        &self,
        design: &Design,
        scene: &ferrocast_core::design::Scene,
        name: &str,
        overlay_id: Option<&str>,
    ) -> Result<OverlayView, EngineError> {
        let mut layers = scene.layers.clone();
        if !design.branded {
            layers.retain(|l| !is_brand_layer(l));
        }
        let link =
            DesignRef { id: design.id.clone(), scene: scene.key.clone(), branded: design.branded, follows: true };
        match overlay_id {
            Some(oid) => {
                let mut o = self.overlay(oid).await?;
                o.width = scene.width;
                o.height = scene.height;
                o.widgets = layers;
                o.design = Some(link);
                let o2 = o.clone();
                let saved = self.db(move |s| repo::overlays::update(s, &o2)).await?;
                let view = self.overlay_view(&saved);
                self.push_config(&saved);
                Ok(view)
            }
            None => {
                let o = Overlay { widgets: layers, ..scene.as_overlay("new", Some(link)) };
                let o = Overlay { name: name.to_string(), ..o };
                let made = self.db(move |s| repo::overlays::create_from(s, o)).await?;
                Ok(self.overlay_view(&made))
            }
        }
    }

    /// Checks an overlay edit against the built-in designs it uses: a built-in keeps its
    /// small Ferrocast mark, on the overlay and on any widget styled by a built-in widget design.
    pub(crate) fn check_branding(&self, current: &Overlay, edited: &Overlay) -> Result<(), EngineError> {
        let keep_mark = || EngineError::Refused {
            status: 409,
            code: "branding",
            message: "This overlay uses a built-in design, which keeps its small Ferrocast mark.".into(),
            hint: "Duplicate the design in the gallery to make an unbranded copy, then use that.".into(),
            retry_after: None,
        };
        if current.design.as_ref().is_some_and(|d| d.branded) && current.has_brand_layer() && !edited.has_brand_layer()
        {
            return Err(keep_mark());
        }
        let builtins = self.builtin_designs();
        for w in &edited.widgets {
            let Some(did) = w.config.get("design_id").and_then(serde_json::Value::as_str) else { continue };
            let Some(b) = builtins.iter().find(|d| d.id == did) else { continue };
            if b.branded && w.config.get("branded").and_then(serde_json::Value::as_bool) != Some(true) {
                return Err(keep_mark());
            }
        }
        Ok(())
    }

    /// Writes a design (and its theme's own alert design) as a `.ferrodesign` file.
    pub async fn export_design(&self, id: &str, out: &Path) -> Result<designfile::Manifest, EngineError> {
        if id.starts_with(BUILTIN_PREFIX) {
            return Err(built_in_refusal("exported"));
        }
        let design = self.design(id).await?;
        let companion = match &design.content {
            DesignContent::Theme(t) => match &t.alert_design {
                Some(a) if !a.starts_with(BUILTIN_PREFIX) => self.design(a).await.ok(),
                _ => None,
            },
            _ => None,
        };
        let out = out.to_path_buf();
        self.db(move |s| {
            designfile::export(s, &design, companion.as_ref(), &out, env!("CARGO_PKG_VERSION")).map_err(|e| match e {
                designfile::Error::Store(s) => s,
                designfile::Error::File(f) => StoreError::Conflict(f.to_string()),
            })
        })
        .await
    }

    /// Imports a `.ferrodesign` as new designs of the streamer's own.
    pub async fn import_design_file(&self, path: &Path) -> Result<designfile::ImportReport, EngineError> {
        let p = path.to_path_buf();
        let store = self.store();
        tokio::task::spawn_blocking(move || designfile::import(&store, &p))
            .await
            .map_err(|e| EngineError::Internal(e.to_string()))?
            .map_err(|e| match e {
                designfile::Error::File(f) => design_file_error(f),
                designfile::Error::Store(s) => EngineError::from(s),
            })
    }

    /// Designs that use a library file (so deleting it can say where it's used).
    pub async fn designs_using_media(&self, media_id: &str) -> Result<Vec<String>, EngineError> {
        let mine = self.db(repo::designs::list).await?;
        Ok(mine.into_iter().filter(|d| d.media_ids().iter().any(|m| m == media_id)).map(|d| d.name).collect())
    }
}

trait WidgetKey {
    fn widget_key(&self) -> &'static str;
}

impl WidgetKey for ferrocast_core::design::WidgetContent {
    fn widget_key(&self) -> &'static str {
        use ferrocast_core::overlay::WidgetKind as K;
        match self.widget {
            K::Alerts => "alerts",
            K::Chat => "chat",
            K::Events => "events",
            K::Goal => "goal",
            K::Label => "label",
            K::Viewers => "viewers",
            K::Poll => "poll",
            K::Giveaway => "giveaway",
            K::Layer => "layer",
        }
    }
}

/// Test alerts that asked for a design, by event id (consumed once).
pub(crate) type TestDesigns = HashMap<String, Design>;

#[cfg(test)]
#[path = "designs_tests.rs"]
mod tests;
