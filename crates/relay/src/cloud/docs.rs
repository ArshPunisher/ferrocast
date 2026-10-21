//! An account's overlays, alert rules, designs and settings in the cloud (docs/CLOUD.md §2),
//! with the engine's rules: the same validators (`Overlay::validate`, `AlertRule::validate`,
//! `Design::validate`), built-ins read-only, a built-in design's mark kept, the alert design
//! in use can't be deleted. Shapes match API.md §4, so the dashboard's screens work as is.

use std::path::Path;

use ferrocast_core::alert::AlertRule;
use ferrocast_core::design::{Design, DesignContent, DesignKind, MAX_NAME, Origin, OriginKind, is_brand_layer};
use ferrocast_core::overlay::{DesignRef, MAX_WIDGETS, Overlay, Preset, Widget, WidgetKind, preset_widgets};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::store::{Doc, Write};
use super::{MAX_DESIGNS, MAX_DOC_BYTES, MAX_OVERLAYS, MAX_RULES, invalid, limit, not_found};
use crate::{ApiError, AppState, now_ms};

/// Built-in design ids all start with this (as in the engine).
pub const BUILTIN_PREFIX: &str = "builtin.";

pub(crate) const OVERLAY: &str = "overlay";
pub(crate) const RULE: &str = "alert_rule";
pub(crate) const DESIGN: &str = "design";
pub(crate) const SETTINGS: &str = "settings";
const MAIN: &str = "main";

/// A new document id: sortable by time, unguessable enough for ids (not secrets).
pub(crate) fn new_id() -> String {
    format!("{:011x}{}", now_ms(), &crate::accounts::store::new_id("")[..12])
}

fn short(name: &str, max: usize) -> String {
    name.trim().chars().take(max).collect::<String>().trim_end().to_string()
}

// ── The built-in catalogue ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Catalog {
    format: String,
    version: u32,
    designs: Vec<Value>,
}

/// Reads the overlay build's catalogue (`designs/catalog.json`); a missing or bad one means
/// no built-ins, never a failed start.
pub(crate) fn load_catalog(path: &Path) -> Vec<Design> {
    let Ok(bytes) = std::fs::read(path) else {
        tracing::warn!(path = %path.display(), "no built-in design catalogue for cloud mode");
        return Vec::new();
    };
    let catalog: Catalog = match serde_json::from_slice(&bytes) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "the built-in design catalogue can't be read");
            return Vec::new();
        }
    };
    if catalog.format != "ferrocast-designs" || catalog.version > 1 {
        tracing::warn!(format = %catalog.format, version = catalog.version, "unknown design catalogue");
        return Vec::new();
    }
    checked_builtins(catalog.designs.into_iter().filter_map(|v| serde_json::from_value(v).ok()).collect())
}

/// Built-ins as the engine keeps them: marked built-in, the right prefix, each once, valid.
pub(crate) fn checked_builtins(list: Vec<Design>) -> Vec<Design> {
    let mut ok = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for mut d in list {
        d.built_in = true;
        if !d.id.starts_with(BUILTIN_PREFIX) || !seen.insert(d.id.clone()) || d.validate().is_err() {
            continue;
        }
        ok.push(d);
    }
    ok
}

// ── Storage helpers ──────────────────────────────────────────────────────────────────

fn read<T: serde::de::DeserializeOwned>(d: &Doc) -> Option<T> {
    d.json.as_deref().and_then(|j| serde_json::from_str(j).ok())
}

fn encode<T: Serialize>(v: &T) -> Result<String, ApiError> {
    let s = serde_json::to_string(v).map_err(|_| invalid("That can't be saved."))?;
    if s.len() > MAX_DOC_BYTES {
        return Err(ApiError::new(axum::http::StatusCode::PAYLOAD_TOO_LARGE, "too_large", "That's over 256 KB.")
            .hint("Use fewer widgets or shorter text."));
    }
    Ok(s)
}

/// Saves without a version check (the web dashboard edits the cloud copy directly).
fn save(st: &AppState, account: &str, kind: &str, id: &str, json: Option<&str>) -> Result<i64, ApiError> {
    match st.db.cloud_write(account, kind, id, json, None, now_ms()).map_err(ApiError::db)? {
        Write::Saved(v) => Ok(v),
        Write::Conflict(_) => {
            Err(super::e(axum::http::StatusCode::CONFLICT, "stale_version", "That changed meanwhile."))
        }
    }
}

fn count(st: &AppState, account: &str, kind: &str) -> Result<i64, ApiError> {
    st.db.cloud_doc_count(account, kind).map_err(ApiError::db)
}

fn live_doc(st: &AppState, account: &str, kind: &str, id: &str) -> Result<Option<Doc>, ApiError> {
    Ok(st.db.cloud_doc(account, kind, id).map_err(ApiError::db)?.filter(|d| d.deleted_at.is_none()))
}

/// What a cloud account starts with: the engine's default alert rules and one Starter
/// overlay (as the app's first run makes).
pub(crate) fn seed(st: &AppState, account: &str) -> Result<(), ApiError> {
    if count(st, account, RULE)? == 0 {
        for r in ferrocast_core::alert::default_rules() {
            save(st, account, RULE, &r.id, Some(&encode(&r)?))?;
        }
    }
    if count(st, account, OVERLAY)? == 0 {
        create_overlay(st, account, "Starter", Preset::Starter)?;
    }
    Ok(())
}

// ── Overlays ─────────────────────────────────────────────────────────────────────────

/// An overlay as the dashboard sees it (API.md §4): the cloud URL instead of a token.
pub(crate) fn overlay_view(st: &AppState, account: &str, o: &Overlay) -> Result<Value, ApiError> {
    Ok(json!({
        "id": o.id, "name": o.name, "url": super::overlay_url(st, account, &o.id)?, "width": o.width,
        "height": o.height, "widgets": o.widgets, "updated_at": o.updated_at, "design": o.design,
    }))
}

pub(crate) fn overlays(st: &AppState, account: &str) -> Result<Vec<Overlay>, ApiError> {
    Ok(st.db.cloud_docs(account, OVERLAY).map_err(ApiError::db)?.iter().filter_map(read).collect())
}

pub(crate) fn overlay(st: &AppState, account: &str, id: &str) -> Result<Overlay, ApiError> {
    live_doc(st, account, OVERLAY, id)?.as_ref().and_then(read).ok_or_else(|| not_found("overlay"))
}

fn store_overlay(st: &AppState, account: &str, o: &mut Overlay) -> Result<(), ApiError> {
    o.token = String::new();
    o.updated_at = now_ms();
    o.validate().map_err(|e| invalid(e.to_string()))?;
    save(st, account, OVERLAY, &o.id, Some(&encode(o)?))?;
    Ok(())
}

pub(crate) fn create_overlay(st: &AppState, account: &str, name: &str, preset: Preset) -> Result<Overlay, ApiError> {
    if count(st, account, OVERLAY)? >= MAX_OVERLAYS {
        return Err(limit(format!("Cloud mode keeps up to {MAX_OVERLAYS} overlays.")).hint("Delete one you don't use."));
    }
    let mut o = Overlay {
        id: new_id(),
        name: short(name, 60),
        token: String::new(),
        width: 1920,
        height: 1080,
        widgets: preset_widgets(preset),
        updated_at: 0,
        design: None,
    };
    store_overlay(st, account, &mut o)?;
    Ok(o)
}

/// Saves an edited overlay (name, size, widgets). Its design link can't be changed here,
/// and a built-in design keeps its mark.
pub(crate) fn update_overlay(st: &AppState, account: &str, id: &str, mut edited: Overlay) -> Result<Overlay, ApiError> {
    let current = overlay(st, account, id)?;
    edited.id = id.to_string();
    edited.design = current.design.clone();
    check_branding(st, &current, &edited)?;
    store_overlay(st, account, &mut edited)?;
    Ok(edited)
}

pub(crate) fn delete_overlay(st: &AppState, account: &str, id: &str) -> Result<(), ApiError> {
    overlay(st, account, id)?;
    save(st, account, OVERLAY, id, None)?;
    // The URL stops, and its generation stays, so an overlay with this id never revives it.
    super::retire_url(st, account, id)?;
    Ok(())
}

/// The engine's `check_branding`: a built-in design keeps its small Ferrocast mark.
fn check_branding(st: &AppState, current: &Overlay, edited: &Overlay) -> Result<(), ApiError> {
    let keep_mark = || {
        super::e(
            axum::http::StatusCode::CONFLICT,
            "branding",
            "This overlay uses a built-in design, which keeps its small Ferrocast mark.",
        )
        .hint("Duplicate the design in the gallery to make an unbranded copy, then use that.")
    };
    if current.design.as_ref().is_some_and(|d| d.branded) && current.has_brand_layer() && !edited.has_brand_layer() {
        return Err(keep_mark());
    }
    let builtins = st.cloud.builtins();
    for w in &edited.widgets {
        let Some(did) = w.config.get("design_id").and_then(Value::as_str) else { continue };
        let Some(b) = builtins.iter().find(|d| d.id == did) else { continue };
        if b.branded && w.config.get("branded").and_then(Value::as_bool) != Some(true) {
            return Err(keep_mark());
        }
    }
    Ok(())
}

// ── Alert rules ──────────────────────────────────────────────────────────────────────

pub(crate) fn rules(st: &AppState, account: &str) -> Result<Vec<AlertRule>, ApiError> {
    Ok(st.db.cloud_docs(account, RULE).map_err(ApiError::db)?.iter().filter_map(read).collect())
}

fn check_rule(r: &AlertRule) -> Result<(), ApiError> {
    if r.id.is_empty() || r.id.len() > 80 || r.id.chars().any(char::is_control) {
        return Err(invalid("id: 1 to 80 characters."));
    }
    r.validate().map_err(|e| invalid(e.to_string()))
}

pub(crate) fn create_rule(st: &AppState, account: &str, mut r: AlertRule) -> Result<AlertRule, ApiError> {
    if r.id.trim().is_empty() {
        r.id = new_id();
    }
    check_rule(&r)?;
    if live_doc(st, account, RULE, &r.id)?.is_some() {
        return Err(super::e(axum::http::StatusCode::CONFLICT, "conflict", "A rule with that id already exists."));
    }
    if count(st, account, RULE)? >= MAX_RULES {
        return Err(limit(format!("Cloud mode keeps up to {MAX_RULES} alert rules.")));
    }
    save(st, account, RULE, &r.id, Some(&encode(&r)?))?;
    Ok(r)
}

pub(crate) fn update_rule(st: &AppState, account: &str, id: &str, mut r: AlertRule) -> Result<AlertRule, ApiError> {
    if live_doc(st, account, RULE, id)?.is_none() {
        return Err(not_found("alert rule"));
    }
    r.id = id.to_string();
    check_rule(&r)?;
    save(st, account, RULE, id, Some(&encode(&r)?))?;
    Ok(r)
}

pub(crate) fn delete_rule(st: &AppState, account: &str, id: &str) -> Result<(), ApiError> {
    if live_doc(st, account, RULE, id)?.is_none() {
        return Err(not_found("alert rule"));
    }
    save(st, account, RULE, id, None)?;
    Ok(())
}

// ── Settings ─────────────────────────────────────────────────────────────────────────

/// The account's few settings (one document).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    /// The alert design every alert plays with (`None`: the rules' own look).
    pub active_alert_design: Option<String>,
    /// Test alerts reach overlays (the app's "test alerts" switch).
    pub test_alerts: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { active_alert_design: None, test_alerts: true }
    }
}

pub(crate) fn settings(st: &AppState, account: &str) -> Result<Settings, ApiError> {
    Ok(live_doc(st, account, SETTINGS, MAIN)?.as_ref().and_then(read).unwrap_or_default())
}

pub(crate) fn save_settings(st: &AppState, account: &str, s: &Settings) -> Result<(), ApiError> {
    save(st, account, SETTINGS, MAIN, Some(&encode(s)?))?;
    Ok(())
}

// ── Designs ──────────────────────────────────────────────────────────────────────────

fn built_in_refusal(action: &str) -> ApiError {
    super::e(axum::http::StatusCode::CONFLICT, "built_in", format!("Built-in designs can't be {action}."))
        .hint("Use Duplicate & customise to make your own copy, then change that.")
}

fn own_designs(st: &AppState, account: &str) -> Result<Vec<Design>, ApiError> {
    let mut list: Vec<Design> =
        st.db.cloud_docs(account, DESIGN).map_err(ApiError::db)?.iter().filter_map(read).collect();
    // Newest first, as the app lists them.
    list.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| a.id.cmp(&b.id)));
    Ok(list)
}

/// Built-ins first (catalogue order), then the account's own, newest first.
pub(crate) fn designs(st: &AppState, account: &str, kind: Option<DesignKind>) -> Result<Vec<Design>, ApiError> {
    let builtins = st.cloud.builtins();
    Ok(builtins
        .iter()
        .cloned()
        .chain(own_designs(st, account)?)
        .filter(|d| kind.is_none_or(|k| d.kind() == k))
        .collect())
}

pub(crate) fn design(st: &AppState, account: &str, id: &str) -> Result<Design, ApiError> {
    if id.starts_with(BUILTIN_PREFIX) {
        return st.cloud.builtins().iter().find(|d| d.id == id).cloned().ok_or_else(|| not_found("design"));
    }
    live_doc(st, account, DESIGN, id)?.as_ref().and_then(read).ok_or_else(|| not_found("design"))
}

fn store_design(st: &AppState, account: &str, d: &Design) -> Result<(), ApiError> {
    d.validate().map_err(|e| invalid(e.to_string()))?;
    save(st, account, DESIGN, &d.id, Some(&encode(d)?))?;
    Ok(())
}

fn room_for_design(st: &AppState, account: &str) -> Result<(), ApiError> {
    if count(st, account, DESIGN)? >= MAX_DESIGNS {
        return Err(limit(format!("Cloud mode keeps up to {MAX_DESIGNS} of your own designs."))
            .hint("Delete designs you don't use."));
    }
    Ok(())
}

/// A new design of the account's own: fresh id, never built-in or branded.
pub(crate) fn create_design(st: &AppState, account: &str, d: Design) -> Result<Design, ApiError> {
    room_for_design(st, account)?;
    let kind = match d.origin.kind {
        OriginKind::Duplicate | OriginKind::AssetPack | OriginKind::Scratch => d.origin.kind,
        OriginKind::BuiltIn | OriginKind::Import => OriginKind::Scratch,
    };
    let from = d.origin.from.as_deref().map(|f| short(f, 120));
    let mine = d.into_user(new_id(), Origin { kind, from }, now_ms());
    store_design(st, account, &mine)?;
    Ok(mine)
}

pub(crate) fn update_design(st: &AppState, account: &str, id: &str, mut d: Design) -> Result<Design, ApiError> {
    if id.starts_with(BUILTIN_PREFIX) {
        return Err(built_in_refusal("changed"));
    }
    let current = design(st, account, id)?;
    d.id = id.to_string();
    d.pro = false;
    if d.built_in || d.branded {
        return Err(invalid("branded: only built-in designs carry the Ferrocast mark."));
    }
    if d.kind() != current.kind() {
        return Err(invalid(format!(
            "kind: a {} design can't become a {} design.",
            current.kind().key(),
            d.kind().key()
        )));
    }
    d.created_at = current.created_at;
    d.updated_at = now_ms();
    store_design(st, account, &d)?;
    Ok(d)
}

pub(crate) fn delete_design(st: &AppState, account: &str, id: &str) -> Result<(), ApiError> {
    if id.starts_with(BUILTIN_PREFIX) {
        return Err(built_in_refusal("deleted"));
    }
    design(st, account, id)?;
    if settings(st, account)?.active_alert_design.as_deref() == Some(id) {
        return Err(super::e(axum::http::StatusCode::CONFLICT, "in_use", "Your alerts play with this design.")
            .hint("Pick another alert design in the gallery first, then delete this one."));
    }
    save(st, account, DESIGN, id, None)?;
    Ok(())
}

/// The account's own copy of any design, without branding (a theme's built-in alert design
/// is copied along).
pub(crate) fn duplicate_design(st: &AppState, account: &str, id: &str, name: Option<&str>) -> Result<Design, ApiError> {
    let source = design(st, account, id)?;
    room_for_design(st, account)?;
    let name = name.map(|n| short(n, MAX_NAME)).filter(|n| !n.is_empty());
    let mut copy = source.clone();
    copy.name = name.unwrap_or_else(|| short(&format!("{} copy", source.name), MAX_NAME));
    if let DesignContent::Theme(t) = &mut copy.content
        && let Some(alert_id) = t.alert_design.clone()
        && alert_id.starts_with(BUILTIN_PREFIX)
    {
        t.alert_design = match design(st, account, &alert_id) {
            Ok(a) => Some(duplicate_design(st, account, &a.id, None)?.id),
            Err(_) => None,
        };
    }
    let origin = Origin { kind: OriginKind::Duplicate, from: Some(short(&source.name, 120)) };
    let mine = copy.into_user(new_id(), origin, now_ms());
    store_design(st, account, &mine)?;
    Ok(mine)
}

pub(crate) fn set_active_alert_design(
    st: &AppState,
    account: &str,
    id: Option<&str>,
) -> Result<Option<String>, ApiError> {
    if let Some(id) = id {
        let d = design(st, account, id)?;
        if d.kind() != DesignKind::Alert {
            return Err(invalid(format!(
                "design_id: \"{}\" is a {} design; pick an alert design.",
                short(&d.name, 40),
                d.kind().key()
            )));
        }
    }
    let mut s = settings(st, account)?;
    s.active_alert_design = id.map(str::to_string);
    save_settings(st, account, &s)?;
    Ok(s.active_alert_design)
}

/// The alert design alerts play with now, if it still exists.
pub(crate) fn active_alert_design(st: &AppState, account: &str) -> Option<Design> {
    let id = settings(st, account).ok()?.active_alert_design?;
    design(st, account, &id).ok().filter(|d| d.kind() == DesignKind::Alert)
}

/// "Use" (API.md §4): the request.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct UseRequest {
    pub overlay_id: Option<String>,
    pub scenes: Vec<String>,
    pub alert_style: Option<bool>,
}

/// "Use": an alert design plays alerts; a theme or scene becomes overlays (or replaces the
/// layout of the one picked, keeping its URL); a widget design adds that widget.
/// `(alert design in use, overlays changed)`.
pub(crate) fn use_design(
    st: &AppState,
    account: &str,
    id: &str,
    req: UseRequest,
) -> Result<(Option<String>, Vec<Overlay>), ApiError> {
    let d = design(st, account, id)?;
    let mut changed = Vec::new();
    let mut alert_design = settings(st, account)?.active_alert_design;
    match &d.content {
        DesignContent::Alert(_) => {
            alert_design = set_active_alert_design(st, account, Some(&d.id))?;
        }
        DesignContent::Theme(t) => {
            let scenes: Vec<_> = if req.scenes.is_empty() {
                t.scenes.iter().collect()
            } else {
                let picked: Vec<_> = t.scenes.iter().filter(|s| req.scenes.contains(&s.key)).collect();
                if picked.len() != req.scenes.len() {
                    return Err(invalid("scenes: one of those scenes isn't in this theme."));
                }
                picked
            };
            if req.overlay_id.is_some() && scenes.len() != 1 {
                return Err(invalid("overlay_id: pick one scene to put on an existing overlay."));
            }
            for scene in scenes {
                let name = short(&format!("{} ({})", scene.name, d.name), 60);
                changed.push(place_scene(st, account, &d, scene, &name, req.overlay_id.as_deref())?);
            }
            if req.alert_style.unwrap_or(true)
                && let Some(a) = &t.alert_design
                && design(st, account, a).is_ok_and(|x| x.kind() == DesignKind::Alert)
            {
                alert_design = set_active_alert_design(st, account, Some(a))?;
            }
        }
        DesignContent::Scene(scene) => {
            let name = short(&d.name, 60);
            changed.push(place_scene(st, account, &d, scene, &name, req.overlay_id.as_deref())?);
        }
        DesignContent::Widget(w) => {
            let Some(oid) = req.overlay_id.as_deref() else {
                return Err(invalid("overlay_id: pick the overlay to add this widget to."));
            };
            let mut o = overlay(st, account, oid)?;
            if o.widgets.len() >= MAX_WIDGETS {
                return Err(super::e(
                    axum::http::StatusCode::CONFLICT,
                    "conflict",
                    format!("That overlay already has {MAX_WIDGETS} widgets, the most one can hold. Remove one first."),
                ));
            }
            let (ww, hh) = (w.w.min(o.width), w.h.min(o.height));
            let key = widget_key(w.widget);
            let n = (1..).find(|i| !o.widgets.iter().any(|x| x.id == format!("{key}-{i}")));
            let wid = format!("{key}-{}", n.unwrap_or(1));
            o.widgets.push(Widget::new(
                &wid,
                w.widget,
                ((o.width - ww) / 2, (o.height - hh) / 2, ww, hh),
                w.config_with_style(&d.id, d.branded),
            ));
            let id = o.id.clone();
            changed.push(update_overlay(st, account, &id, o)?);
        }
    }
    Ok((alert_design, changed))
}

fn widget_key(k: WidgetKind) -> &'static str {
    match k {
        WidgetKind::Alerts => "alerts",
        WidgetKind::Chat => "chat",
        WidgetKind::Events => "events",
        WidgetKind::Goal => "goal",
        WidgetKind::Label => "label",
        WidgetKind::Viewers => "viewers",
        WidgetKind::Poll => "poll",
        WidgetKind::Giveaway => "giveaway",
        WidgetKind::Layer => "layer",
    }
}

fn place_scene(
    st: &AppState,
    account: &str,
    d: &Design,
    scene: &ferrocast_core::design::Scene,
    name: &str,
    overlay_id: Option<&str>,
) -> Result<Overlay, ApiError> {
    let mut layers = scene.layers.clone();
    if !d.branded {
        layers.retain(|l| !is_brand_layer(l));
    }
    let link = DesignRef { id: d.id.clone(), scene: scene.key.clone(), branded: d.branded };
    match overlay_id {
        Some(oid) => {
            let mut o = overlay(st, account, oid)?;
            o.width = scene.width;
            o.height = scene.height;
            o.widgets = layers;
            o.design = Some(link);
            store_overlay(st, account, &mut o)?;
            Ok(o)
        }
        None => {
            if count(st, account, OVERLAY)? >= MAX_OVERLAYS {
                return Err(limit(format!("Cloud mode keeps up to {MAX_OVERLAYS} overlays.")));
            }
            let mut o = Overlay { widgets: layers, ..scene.as_overlay(&new_id(), Some(link)) };
            o.name = name.to_string();
            store_overlay(st, account, &mut o)?;
            Ok(o)
        }
    }
}

// ── Sync (docs/CLOUD.md §1.1) ────────────────────────────────────────────────────────

/// Checks a document the desktop pushes, as the cloud would store it (the app's local
/// overlay token is never kept). `Err` is the message naming the problem.
pub(crate) fn check_pushed(st: &AppState, account: &str, kind: &str, id: &str, body: &Value) -> Result<String, String> {
    let fix_id = |mut v: Value| {
        v["id"] = json!(id);
        v
    };
    match kind {
        OVERLAY => {
            let mut v = fix_id(body.clone());
            v["token"] = json!("");
            let mut o: Overlay = serde_json::from_value(v).map_err(|e| format!("not an overlay: {e}"))?;
            o.validate().map_err(|e| e.to_string())?;
            // Branding is checked against the cloud's own copy (as the web's PUT does), and its
            // design link stays; a new overlay is checked on its own.
            let stored = live_doc(st, account, OVERLAY, id).map_err(|e| e.message().to_string())?;
            let current = match stored.as_ref().and_then(read::<Overlay>) {
                Some(c) => {
                    o.design = c.design.clone();
                    c
                }
                None => Overlay { design: None, ..o.clone() },
            };
            check_branding(st, &current, &o).map_err(|e| e.message().to_string())?;
            serde_json::to_string(&o).map_err(|e| e.to_string())
        }
        RULE => {
            let r: AlertRule =
                serde_json::from_value(fix_id(body.clone())).map_err(|e| format!("not an alert rule: {e}"))?;
            check_rule(&r).map_err(|e| e.message().to_string())?;
            serde_json::to_string(&r).map_err(|e| e.to_string())
        }
        DESIGN => {
            if id.starts_with(BUILTIN_PREFIX) {
                return Err("built-in designs aren't synced".into());
            }
            let mut d: Design =
                serde_json::from_value(fix_id(body.clone())).map_err(|e| format!("not a design: {e}"))?;
            // Only built-ins are ever Pro (API.md §4 Designs).
            d.pro = false;
            if d.built_in || d.branded {
                return Err("branded: only built-in designs carry the Ferrocast mark".into());
            }
            d.validate().map_err(|e| e.to_string())?;
            serde_json::to_string(&d).map_err(|e| e.to_string())
        }
        SETTINGS => {
            if id != MAIN {
                return Err("settings: the id is \"main\"".into());
            }
            let s: Settings = serde_json::from_value(body.clone()).map_err(|e| format!("not settings: {e}"))?;
            serde_json::to_string(&s).map_err(|e| e.to_string())
        }
        _ => Err(format!("kind: \"{kind}\" isn't one of overlay, alert_rule, design, settings")),
    }
}

/// Whether `adding` new documents of a kind would go over its cap.
pub(crate) fn over_cap(st: &AppState, account: &str, kind: &str, adding: i64) -> Result<bool, ApiError> {
    let cap = match kind {
        OVERLAY => MAX_OVERLAYS,
        RULE => MAX_RULES,
        DESIGN => MAX_DESIGNS,
        _ => return Ok(false),
    };
    Ok(count(st, account, kind)? + adding > cap)
}
