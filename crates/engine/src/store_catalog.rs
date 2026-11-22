//! Community Theme Store catalog & installer (Phase 3 #18, R37).
//!
//! Fetches a minisign-signed catalog JSON (checking signature & byte bounds before parsing),
//! validates creators, items, URLs, paths, hashes, and p2e custom alert HTML/CSS, and installs
//! pure-data `.ferrodesign` archives, sandboxed `.ferroalert` bundles, or sandboxed `.zip`
//! plugins with persistent one-click undo. Zero usage telemetry is ever transmitted.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine as _;
use ferrocast_core::alert::{ALERT_KINDS, Variation, default_rules, validate_variation};
use ferrocast_core::design::{Design, EventKey, MotionTag, StyleTag};
use ferrocast_core::layer::is_color;
use ferrocast_store::repo::store_catalog::{
    PluginSnapshot, StoreInstallRecord, StoreUndoEntry, StoreUndoSnapshot, StoreUndoSummary,
};
use ferrocast_store::{designfile, repo};
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::plugin_catalog::{Source, client as http_client, fetch_capped, file_url, hex_bytes, newer, verify};
use crate::plugins::{ACTIONS as PLUGIN_ACTIONS, MAX_ZIP as MAX_PLUGIN_ZIP, read_zip as read_plugin_zip};
use crate::{Engine, EngineError, now};

/// Default signed Community Store catalog endpoint (companion `.sig` lives beside it).
pub const STORE_CATALOG_URL: &str = "https://raw.githubusercontent.com/ArshPunisher/ferrocast-store/main/catalog.json";

/// Public key baked into the app for Community Store catalog verification. Distinct from any test
/// fixture public key or app updater key. Empty closes the catalog cleanly (`503 catalog_not_open`).
pub const STORE_CATALOG_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDI4QTNGQTgzQjgxRjc3QTkKUldTcGR4KzRnL3FqS0tEYmlsQlJJNlFhN3hXQTAzaWFvVUpudmRQaC90a1h5M1UweThwY0hxNVEK";

/// Hard security bounds on catalog and package reads.
pub const MAX_CATALOG_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_SIG_BYTES: usize = 4 * 1024;
pub const MAX_CREATORS: usize = 500;
pub const MAX_ITEMS: usize = 1_000;
pub const MAX_DESIGN_PACKAGE_BYTES: usize = designfile::MAX_FILE as usize;
pub const MAX_ALERT_PACKAGE_BYTES: usize = 512 * 1024;
pub const MAX_PLUGIN_PACKAGE_BYTES: usize = MAX_PLUGIN_ZIP as usize;
pub const MAX_ASSET_BYTES: usize = 10 * 1024 * 1024;
pub const CACHE_TTL: Duration = Duration::from_secs(600);
pub const STALE_AFTER_MS: i64 = 7 * 24 * 60 * 60 * 1000;

static STORE_SOURCE: Mutex<Option<Source>> = Mutex::new(None);
static STORE_CACHE: Mutex<Option<CachedCatalog>> = Mutex::new(None);

#[derive(Debug, Clone)]
struct CachedCatalog {
    at: Instant,
    fetched_at: i64,
    catalog: VerifiedCatalog,
}

#[derive(Debug, Clone)]
struct VerifiedCatalog {
    generated_at: i64,
    creators: Vec<StoreCreator>,
    items: Vec<StoreItemRaw>,
    warnings: Vec<String>,
}

/// Overrides the Community Store catalog URL/key (tests only).
pub fn set_source(s: Source) {
    *STORE_SOURCE.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
    *STORE_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Clears any test override for the Community Store catalog.
pub fn clear_source() {
    *STORE_SOURCE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *STORE_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn current_source() -> Source {
    STORE_SOURCE.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(|| Source {
        url: STORE_CATALOG_URL.into(),
        key: STORE_CATALOG_KEY.into(),
        local: false,
    })
}

fn refused(status: u16, code: &'static str, message: impl Into<String>, hint: impl Into<String>) -> EngineError {
    EngineError::Refused { status, code, message: message.into(), hint: hint.into(), retry_after: None }
}

// ------------------------------------------------------------------------------- catalog shapes

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreCategory {
    Theme,
    Alert,
    Widget,
    Overlay,
    Plugin,
}

impl StoreCategory {
    pub fn key(self) -> &'static str {
        match self {
            StoreCategory::Theme => "theme",
            StoreCategory::Alert => "alert",
            StoreCategory::Widget => "widget",
            StoreCategory::Overlay => "overlay",
            StoreCategory::Plugin => "plugin",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorePackageFormat {
    Ferrodesign,
    Ferroalert,
    Ferroplugin,
}

impl StorePackageFormat {
    pub fn key(self) -> &'static str {
        match self {
            StorePackageFormat::Ferrodesign => "ferrodesign",
            StorePackageFormat::Ferroalert => "ferroalert",
            StorePackageFormat::Ferroplugin => "ferroplugin",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StoreCreatorLinks {
    pub website: Option<String>,
    pub twitch: Option<String>,
    pub youtube: Option<String>,
    pub github: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreCreator {
    pub id: String,
    pub name: String,
    pub handle: String,
    #[serde(default)]
    pub bio: String,
    #[serde(default)]
    pub verified: bool,
    #[serde(default)]
    pub links: StoreCreatorLinks,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorePackage {
    pub format: StorePackageFormat,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreAsset {
    pub name: String,
    pub role: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub mime: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoreStats {
    pub installs: u64,
    pub rating_avg: f64,
    pub rating_count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StorePreviewSpec {
    pub palette: [String; 3],
    pub style: StyleTag,
    pub motion: MotionTag,
    #[serde(default)]
    pub headline: Option<String>,
    #[serde(default)]
    pub subline: Option<String>,
    #[serde(default)]
    pub layers_hint: Option<u32>,
    #[serde(default)]
    pub design: Option<Design>,
    #[serde(default)]
    pub custom_alert: Option<StoreCustomAlertPreview>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoreCustomAlertPreview {
    pub html: String,
    pub css: String,
    #[serde(default)]
    pub fields: Vec<ferrocast_core::alert::CustomField>,
    #[serde(default)]
    pub field_values: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct StoreItemRaw {
    pub id: String,
    pub kind: StoreCategory,
    pub title: String,
    pub summary: String,
    pub version: String,
    pub creator_id: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub license: String,
    #[serde(default)]
    pub sandboxed: bool,
    #[serde(default)]
    pub plugin_kind: Option<String>,
    #[serde(default)]
    pub plugin_actions: Vec<String>,
    #[serde(default)]
    pub alert_events: Vec<EventKey>,
    #[serde(default)]
    pub featured: bool,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    pub package: StorePackage,
    #[serde(default)]
    pub assets: Vec<StoreAsset>,
    pub stats: StoreStats,
    pub preview: StorePreviewSpec,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StoreItem {
    pub id: String,
    pub kind: StoreCategory,
    pub title: String,
    pub summary: String,
    pub version: String,
    pub creator_id: String,
    pub tags: Vec<String>,
    pub license: String,
    pub sandboxed: bool,
    pub plugin_kind: Option<String>,
    pub plugin_actions: Vec<String>,
    pub alert_events: Vec<EventKey>,
    pub featured: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub package: StorePackage,
    pub assets: Vec<StoreAsset>,
    pub stats: StoreStats,
    pub preview: StorePreviewSpec,
    pub installed_version: Option<String>,
    pub update_available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoreInstalledItem {
    pub item_id: String,
    pub kind: StoreCategory,
    pub format: StorePackageFormat,
    pub title: String,
    pub version: String,
    pub creator_id: String,
    pub creator_name: String,
    pub installed_at: i64,
    pub updated_at: i64,
    pub package_sha256: String,
    pub design_ids: Vec<String>,
    pub rule_ids: Vec<String>,
    pub plugin_id: Option<String>,
    pub can_undo: bool,
    pub undo_token: Option<String>,
    pub latest_version: Option<String>,
    pub update_available: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StoreCatalogView {
    pub version: u32,
    pub generated_at: i64,
    pub fetched_at: i64,
    pub stale: bool,
    pub creators: Vec<StoreCreator>,
    pub items: Vec<StoreItem>,
    pub installed: Vec<StoreInstalledItem>,
    pub last_undo: Option<StoreUndoSummary>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoreInstalledView {
    pub installed: Vec<StoreInstalledItem>,
    pub last_undo: Option<StoreUndoSummary>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StoreMutationResult {
    pub action: String,
    pub installed: Option<StoreInstalledItem>,
    pub designs: Vec<Design>,
    pub undo: StoreUndoSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoreUndoResult {
    pub reverted_action: String,
    pub item_id: String,
    pub item_title: String,
    pub installed: Vec<StoreInstalledItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StoreLinkPreview {
    pub uri: String,
    pub action: String,
    pub item: StoreItem,
    pub creator: StoreCreator,
    pub installed: Option<StoreInstalledItem>,
    pub update_available: bool,
    pub safety_notes: Vec<String>,
    pub requires_confirmation: bool,
}

/// Custom alert bundle (`format = "ferroalert"`) validated strictly via p2e sandbox rules.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoreAlertBundle {
    pub schema: u32,
    pub id: String,
    pub name: String,
    pub events: Vec<EventKey>,
    pub variations: Vec<Variation>,
}

// ------------------------------------------------------------------------------- validation

pub(crate) fn good_store_id(id: &str) -> bool {
    (3..=80).contains(&id.len())
        && id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'))
        && !id.contains("..")
}

fn check_plain_text(field: &str, s: &str, max: usize, allow_empty: bool) -> Result<(), String> {
    let trimmed = s.trim();
    if !allow_empty && trimmed.is_empty() {
        return Err(format!("{field}: can't be empty"));
    }
    if s.chars().count() > max {
        return Err(format!("{field}: at most {max} characters"));
    }
    if s.chars().any(|c| (c.is_control() && c != '\n') || c == '<' || c == '>') {
        return Err(format!("{field}: HTML or control characters aren't allowed"));
    }
    Ok(())
}

fn check_version(field: &str, v: &str) -> Result<(), String> {
    let ok = (1..=32).contains(&v.len())
        && v.starts_with(|c: char| c.is_ascii_digit())
        && v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'));
    if ok { Ok(()) } else { Err(format!("{field}: bad version \"{v}\"")) }
}

fn check_sha256(field: &str, h: &str) -> Result<(), String> {
    let ok = h.len() == 64 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if ok { Ok(()) } else { Err(format!("{field}: sha256 must be 64 lowercase hex characters")) }
}

fn check_creator_link(field: &str, raw: &str) -> Result<(), String> {
    if raw.len() > 240 || raw.chars().any(|c| c.is_control() || c.is_whitespace() || c == '<' || c == '>') {
        return Err(format!("{field}: invalid link"));
    }
    let u = reqwest::Url::parse(raw).map_err(|_| format!("{field}: unreadable URL"))?;
    if u.scheme() != "https" || !u.username().is_empty() || u.password().is_some() {
        return Err(format!("{field}: links must use https without credentials"));
    }
    let Some(host) = u.host_str() else {
        return Err(format!("{field}: missing host"));
    };
    let lower = host.to_ascii_lowercase();
    if lower == "localhost"
        || lower.ends_with(".local")
        || lower.parse::<std::net::IpAddr>().is_ok_and(|ip| match ip {
            std::net::IpAddr::V4(v4) => {
                v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
            }
            std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
        })
    {
        return Err(format!("{field}: private or loopback hosts aren't allowed"));
    }
    Ok(())
}

fn check_creator(c: &StoreCreator) -> Result<(), String> {
    if !good_store_id(&c.id) {
        return Err(format!("creator id \"{}\": invalid format", c.id));
    }
    check_plain_text("creator.name", &c.name, 60, false)?;
    check_plain_text("creator.handle", &c.handle, 44, false)?;
    if !c.handle.starts_with('@') || c.handle[1..].contains(char::is_whitespace) {
        return Err(format!("creator.handle: \"{}\" must start with @ without spaces", c.handle));
    }
    check_plain_text("creator.bio", &c.bio, 280, true)?;
    for (k, v) in [
        ("links.website", &c.links.website),
        ("links.twitch", &c.links.twitch),
        ("links.youtube", &c.links.youtube),
        ("links.github", &c.links.github),
    ] {
        if let Some(url) = v {
            check_creator_link(k, url)?;
        }
    }
    Ok(())
}

fn check_asset_mime(mime: &str) -> bool {
    matches!(mime, "image/png" | "image/jpeg" | "image/webp" | "image/gif" | "audio/mpeg" | "audio/ogg" | "audio/wav")
}

fn max_package_size(format: StorePackageFormat) -> u64 {
    match format {
        StorePackageFormat::Ferrodesign => MAX_DESIGN_PACKAGE_BYTES as u64,
        StorePackageFormat::Ferroalert => MAX_ALERT_PACKAGE_BYTES as u64,
        StorePackageFormat::Ferroplugin => MAX_PLUGIN_PACKAGE_BYTES as u64,
    }
}

fn validate_item_raw(
    item: &StoreItemRaw,
    creators: &HashMap<String, StoreCreator>,
    src: &Source,
) -> Result<(), String> {
    if !good_store_id(&item.id) {
        return Err(format!("item id \"{}\": invalid format", item.id));
    }
    if !creators.contains_key(&item.creator_id) {
        return Err(format!("creator_id \"{}\": unknown creator", item.creator_id));
    }
    check_plain_text("title", &item.title, 60, false)?;
    check_plain_text("summary", &item.summary, 200, false)?;
    check_version("version", &item.version)?;
    check_plain_text("license", &item.license, 40, false)?;
    if item.tags.len() > 8 {
        return Err("tags: at most 8 tags".into());
    }
    for tag in &item.tags {
        let ok = (1..=24).contains(&tag.len())
            && tag.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !ok {
            return Err(format!("tags: bad tag \"{tag}\""));
        }
    }
    // Format vs kind pairing.
    match (item.kind, item.package.format) {
        (StoreCategory::Theme | StoreCategory::Widget | StoreCategory::Overlay, StorePackageFormat::Ferrodesign)
        | (StoreCategory::Alert, StorePackageFormat::Ferrodesign | StorePackageFormat::Ferroalert)
        | (StoreCategory::Plugin, StorePackageFormat::Ferroplugin) => {}
        _ => return Err(format!("kind {:?} cannot use package format {:?}", item.kind, item.package.format)),
    }
    if item.kind == StoreCategory::Plugin {
        let pk = item.plugin_kind.as_deref().unwrap_or("");
        if !matches!(pk, "effect" | "logic" | "panel") {
            return Err(format!("plugin_kind \"{pk}\": must be effect, logic or panel"));
        }
        if pk == "effect" && !item.plugin_actions.is_empty() {
            return Err("plugin_actions: effect shaders cannot request actions".into());
        }
        if let Some(bad) = item.plugin_actions.iter().find(|a| !PLUGIN_ACTIONS.contains(&a.as_str())) {
            return Err(format!("plugin_actions: \"{bad}\" isn't allowed"));
        }
    }
    // Package verification fields.
    if file_url(&item.package.url, src).is_none() {
        return Err(format!("package.url \"{}\": disallowed URL or path", item.package.url));
    }
    check_sha256("package.sha256", &item.package.sha256)?;
    let max_pkg = max_package_size(item.package.format);
    if !(1..=max_pkg).contains(&item.package.size) {
        return Err(format!("package.size {}: must be 1..={max_pkg}", item.package.size));
    }
    if item.assets.len() > 16 {
        return Err("assets: at most 16 companion assets".into());
    }
    let mut asset_names = HashSet::new();
    for a in &item.assets {
        check_plain_text("asset.name", &a.name, 64, false)?;
        if a.name.contains(['/', '\\']) || a.name == "." || a.name == ".." || !asset_names.insert(a.name.clone()) {
            return Err(format!("asset.name \"{}\": invalid or duplicate", a.name));
        }
        if !matches!(a.role.as_str(), "preview" | "media") {
            return Err(format!("asset.role \"{}\": invalid", a.role));
        }
        if file_url(&a.url, src).is_none() {
            return Err(format!("asset.url \"{}\": disallowed URL or path", a.url));
        }
        check_sha256("asset.sha256", &a.sha256)?;
        if !(1..=MAX_ASSET_BYTES as u64).contains(&a.size) {
            return Err(format!("asset.size {}: must be 1..={MAX_ASSET_BYTES}", a.size));
        }
        if !check_asset_mime(&a.mime) {
            return Err(format!("asset.mime \"{}\": unsupported", a.mime));
        }
    }
    // Read-only catalog stats.
    if !item.stats.rating_avg.is_finite() || !(0.0..=5.0).contains(&item.stats.rating_avg) {
        return Err("stats.rating_avg: must be 0.0..=5.0".into());
    }
    // Preview validation.
    for col in &item.preview.palette {
        if !is_color(col) {
            return Err(format!("preview.palette: \"{col}\" isn't a valid colour"));
        }
    }
    if let Some(h) = &item.preview.headline {
        check_plain_text("preview.headline", h, 80, true)?;
    }
    if let Some(s) = &item.preview.subline {
        check_plain_text("preview.subline", s, 120, true)?;
    }
    if let Some(d) = &item.preview.design {
        if d.built_in || d.branded {
            return Err("preview.design: cannot set built_in or branded".into());
        }
        d.validate().map_err(|e| format!("preview.design: {e}"))?;
    }
    if let Some(ca) = &item.preview.custom_alert {
        check_custom_alert_preview(&item.id, ca).map_err(|e| format!("preview.custom_alert: {e}"))?;
    }
    Ok(())
}

fn check_custom_alert_preview(id: &str, ca: &StoreCustomAlertPreview) -> Result<(), String> {
    let mut v = Variation::new(id, "{name}");
    v.name = id.into();
    v.custom_enabled = true;
    v.html = ca.html.clone();
    v.css = ca.css.clone();
    v.fields = ca.fields.clone();
    v.field_values = ca.field_values.clone();
    validate_variation(&v).map_err(|e| e.to_string())
}

/// Parses and validates a signed catalog JSON buffer. Root schema corruption returns `Err`, while
/// individual bad entries are dropped cleanly with warnings so valid entries stay usable.
fn parse_and_validate_catalog(bytes: &[u8], src: &Source) -> Result<VerifiedCatalog, EngineError> {
    #[derive(Deserialize)]
    struct RawCatalog {
        version: u32,
        generated_at: i64,
        creators: Vec<serde_json::Value>,
        items: Vec<serde_json::Value>,
    }
    let raw: RawCatalog = serde_json::from_slice(bytes).map_err(|e| {
        refused(
            422,
            "store_catalog_invalid",
            format!("The Community Store catalog isn't readable: {e}"),
            "Installed items still work cleanly.",
        )
    })?;
    if raw.version != 1 {
        return Err(refused(
            422,
            "store_catalog_invalid",
            format!("The Community Store catalog format ({}) isn't supported by this Ferrocast.", raw.version),
            "Update Ferrocast to browse newer catalogs; installed items still work.",
        ));
    }
    if raw.creators.len() > MAX_CREATORS || raw.items.len() > MAX_ITEMS {
        return Err(refused(
            413,
            "store_catalog_too_large",
            "The Community Store catalog has more entries than allowed.",
            "Try again later; installed items still work.",
        ));
    }
    let mut warnings = Vec::new();
    let mut creators_map: HashMap<String, StoreCreator> = HashMap::new();
    let mut creators = Vec::new();
    for val in raw.creators {
        match serde_json::from_value::<StoreCreator>(val) {
            Ok(c) => match check_creator(&c) {
                Ok(()) if !creators_map.contains_key(&c.id) => {
                    creators_map.insert(c.id.clone(), c.clone());
                    creators.push(c);
                }
                Ok(()) => warnings.push(format!("Skipped duplicate creator '{}'.", c.id)),
                Err(why) => warnings.push(format!("Skipped invalid creator '{}': {why}", c.id)),
            },
            Err(why) => warnings.push(format!("Skipped unreadable creator entry: {why}")),
        }
    }
    let mut seen_ids = HashSet::new();
    let mut items = Vec::new();
    for val in raw.items {
        let label = val.get("id").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
        match serde_json::from_value::<StoreItemRaw>(val) {
            Ok(item) => {
                if !seen_ids.insert(item.id.clone()) {
                    warnings.push(format!("Skipped duplicate item '{}'.", item.id));
                    continue;
                }
                match validate_item_raw(&item, &creators_map, src) {
                    Ok(()) => items.push(item),
                    Err(why) => warnings.push(format!("Skipped unsafe item '{}': {why}", item.id)),
                }
            }
            Err(why) => warnings.push(format!("Skipped unreadable item '{label}': {why}")),
        }
    }
    Ok(VerifiedCatalog { generated_at: raw.generated_at, creators, items, warnings })
}

// ------------------------------------------------------------------------------- deep link parsing

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedStoreLink {
    pub uri: String,
    pub action: String,
    pub item_id: String,
    pub version: Option<String>,
}

/// Strictly parses a `ferrocast://` Store link (`install` or `view`). Never triggers an install.
pub fn parse_ferrocast_link(raw: &str) -> Result<ParsedStoreLink, EngineError> {
    if raw.len() > 512 || raw.chars().any(char::is_control) || raw != raw.trim() || raw.contains('\\') {
        return Err(EngineError::Invalid("uri: malformed Ferrocast link.".into()));
    }
    let u = reqwest::Url::parse(raw).map_err(|_| EngineError::Invalid("uri: unreadable Ferrocast link.".into()))?;
    if u.scheme() != "ferrocast" || !u.username().is_empty() || u.password().is_some() {
        return Err(EngineError::Invalid("uri: link must start with ferrocast://.".into()));
    }
    let host = u.host_str().unwrap_or("");
    let segs: Vec<&str> = u.path().trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    let (action, id) = match (host, segs.as_slice()) {
        ("store", ["install", id]) | ("install", [id]) => ("install", *id),
        ("store", ["item", id]) => ("view", *id),
        _ => {
            return Err(EngineError::Invalid(
                "uri: expected ferrocast://store/install/<id> or ferrocast://store/item/<id>.".into(),
            ));
        }
    };
    if !good_store_id(id) || u.path().contains('%') {
        return Err(EngineError::Invalid("uri: that item id isn't valid.".into()));
    }
    let mut version = None;
    for (k, v) in u.query_pairs() {
        if k == "v" {
            check_version("uri.v", &v).map_err(EngineError::Invalid)?;
            version = Some(v.into_owned());
        } else {
            return Err(EngineError::Invalid(format!("uri: unexpected parameter \"{k}\".")));
        }
    }
    Ok(ParsedStoreLink { uri: raw.to_string(), action: action.to_string(), item_id: id.to_string(), version })
}

// ------------------------------------------------------------------------------- helper conversions

fn parse_category(s: &str) -> StoreCategory {
    match s {
        "theme" => StoreCategory::Theme,
        "alert" => StoreCategory::Alert,
        "widget" => StoreCategory::Widget,
        "overlay" => StoreCategory::Overlay,
        "plugin" => StoreCategory::Plugin,
        _ => StoreCategory::Theme,
    }
}

fn parse_format(s: &str) -> StorePackageFormat {
    match s {
        "ferroalert" => StorePackageFormat::Ferroalert,
        "ferroplugin" => StorePackageFormat::Ferroplugin,
        _ => StorePackageFormat::Ferrodesign,
    }
}

fn event_key_str(k: EventKey) -> &'static str {
    match k {
        EventKey::Follow => "follow",
        EventKey::Sub => "subscription",
        EventKey::Resub => "subscription",
        EventKey::Gift => "gift_batch",
        EventKey::Member => "subscription",
        EventKey::Bits | EventKey::Kicks => "platform_gift",
        EventKey::Tip => "tip",
        EventKey::Superchat => "paid_message",
        EventKey::Raid => "raid",
        EventKey::Redeem => "reward_redemption",
    }
}

fn check_alert_bundle(bytes: &[u8]) -> Result<StoreAlertBundle, EngineError> {
    let bundle: StoreAlertBundle = serde_json::from_slice(bytes).map_err(|e| {
        refused(422, "store_unsafe_package", format!("The alert package isn't readable JSON: {e}"), "Try another item.")
    })?;
    if bundle.schema != 1 {
        return Err(refused(
            422,
            "store_unsafe_package",
            format!("Alert bundle schema {} isn't supported.", bundle.schema),
            "Update Ferrocast first.",
        ));
    }
    if !good_store_id(&bundle.id) || check_plain_text("name", &bundle.name, 60, false).is_err() {
        return Err(refused(
            422,
            "store_unsafe_package",
            "The alert bundle metadata isn't valid.",
            "Report this item to the catalog maintainers.",
        ));
    }
    if bundle.events.is_empty()
        || bundle.variations.is_empty()
        || bundle.variations.len() > ferrocast_core::alert::MAX_VARIATIONS
    {
        return Err(refused(
            422,
            "store_unsafe_package",
            "The alert bundle needs 1 to 20 variations and at least one event.",
            "Report this item to the catalog maintainers.",
        ));
    }
    for ev in &bundle.events {
        let name = event_key_str(*ev);
        if !ALERT_KINDS.contains(&name) {
            return Err(refused(422, "store_unsafe_package", "Unsupported alert event.", "Try another item."));
        }
    }
    for v in &bundle.variations {
        validate_variation(v).map_err(|e| {
            refused(
                422,
                "store_unsafe_package",
                format!("Unsafe custom alert blocked by sandbox: {e}"),
                "Store alerts may only use safe HTML, CSS and known tokens.",
            )
        })?;
    }
    Ok(bundle)
}

fn verified_error_map(e: EngineError, default_code: &'static str) -> EngineError {
    match e {
        EngineError::Refused { status: 413, message, hint, .. } => refused(413, default_code, message, hint),
        EngineError::Refused { status: 502, code: "catalog_unreachable", message, hint, .. } => {
            refused(502, "store_unreachable", message, hint)
        }
        other => other,
    }
}

fn snapshot_plugin_zip(dir: &Path, id: &str) -> Result<Option<String>, EngineError> {
    let plugin_dir = dir.join(id);
    if !plugin_dir.is_dir() {
        return Ok(None);
    }
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zw = zip::ZipWriter::new(&mut buf);
        let entries = std::fs::read_dir(&plugin_dir).map_err(|e| EngineError::Internal(e.to_string()))?;
        for entry in entries {
            let entry = entry.map_err(|e| EngineError::Internal(e.to_string()))?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue };
            let mut file_bytes = Vec::new();
            std::fs::File::open(&path)
                .and_then(|mut f| f.read_to_end(&mut file_bytes))
                .map_err(|e| EngineError::Internal(e.to_string()))?;
            zw.start_file(name, zip::write::SimpleFileOptions::default())
                .map_err(|e| EngineError::Internal(e.to_string()))?;
            zw.write_all(&file_bytes).map_err(|e| EngineError::Internal(e.to_string()))?;
        }
        zw.finish().map_err(|e| EngineError::Internal(e.to_string()))?;
    }
    Ok(Some(base64::engine::general_purpose::STANDARD.encode(buf.into_inner())))
}

fn installed_view(
    rec: &StoreInstallRecord,
    raw_items: Option<&[StoreItemRaw]>,
    undo: Option<&StoreUndoSummary>,
) -> StoreInstalledItem {
    let catalog_item = raw_items.and_then(|items| items.iter().find(|i| i.id == rec.item_id));
    let latest_version = catalog_item.map(|i| i.version.clone());
    let update_available = latest_version.as_deref().is_some_and(|v| newer(v, &rec.version));
    let undo_token = undo.filter(|u| u.item_id == rec.item_id).map(|u| u.token.clone());
    StoreInstalledItem {
        item_id: rec.item_id.clone(),
        kind: parse_category(&rec.kind),
        format: parse_format(&rec.format),
        title: rec.title.clone(),
        version: rec.version.clone(),
        creator_id: rec.creator_id.clone(),
        creator_name: rec.creator_name.clone(),
        installed_at: rec.installed_at,
        updated_at: rec.updated_at,
        package_sha256: rec.package_sha256.clone(),
        design_ids: rec.design_ids.clone(),
        rule_ids: rec.rule_ids.clone(),
        plugin_id: rec.plugin_id.clone(),
        can_undo: undo_token.is_some(),
        undo_token,
        latest_version,
        update_available,
    }
}

fn merge_variations(existing: &[Variation], item_id: &str, incoming: &[Variation]) -> Vec<Variation> {
    let prefix = format!("store:{item_id}:");
    let mut out: Vec<Variation> = existing.iter().filter(|v| !v.id.starts_with(&prefix)).cloned().collect();
    // If the incoming pack supplies a default variation (condition: None), demote or replace
    // cleanly so the rule keeps exactly one default variation.
    let has_default = incoming.iter().any(|v| v.condition.is_none());
    if has_default {
        out.retain(|v| v.condition.is_some());
    }
    for v in incoming {
        let mut tagged = v.clone();
        tagged.id = format!("{prefix}{}", v.id);
        out.push(tagged);
    }
    out
}

// ------------------------------------------------------------------------------- Engine API

impl Engine {
    async fn fetch_verified_catalog(&self, fresh: bool) -> Result<(VerifiedCatalog, i64, Source), EngineError> {
        let src = current_source();
        if !fresh
            && let Some(c) = STORE_CACHE.lock().unwrap_or_else(|e| e.into_inner()).clone()
            && c.at.elapsed() < CACHE_TTL
        {
            return Ok((c.catalog, c.fetched_at, src));
        }
        if src.key.trim().is_empty() {
            return Err(refused(
                503,
                "catalog_not_open",
                "The Community Store catalog isn't open yet.",
                "Installed designs and plugins keep working.",
            ));
        }
        let url = reqwest::Url::parse(&src.url).map_err(|e| EngineError::Internal(e.to_string()))?;
        let mut sig_url = url.clone();
        sig_url.set_path(&format!("{}.sig", url.path()));
        let http = http_client(&src)?;
        let list = fetch_capped(&http, url, MAX_CATALOG_BYTES)
            .await
            .map_err(|e| verified_error_map(e, "store_catalog_too_large"))?;
        let sig_bytes = fetch_capped(&http, sig_url, MAX_SIG_BYTES)
            .await
            .map_err(|e| verified_error_map(e, "store_catalog_too_large"))?;
        let sig = String::from_utf8(sig_bytes).unwrap_or_default();
        if let Err(why) = verify(&list, &sig, &src.key) {
            tracing::warn!(%why, "community store catalog failed minisign check");
            return Err(refused(
                502,
                "store_bad_signature",
                "The Community Store catalog signature isn't valid, so it wasn't opened.",
                "Installed items stay safe and working. Try refreshing later.",
            ));
        }
        let verified = parse_and_validate_catalog(&list, &src)?;
        let fetched_at = now().0;
        *STORE_CACHE.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(CachedCatalog { at: Instant::now(), fetched_at, catalog: verified.clone() });
        Ok((verified, fetched_at, src))
    }

    async fn fetch_verified_file(
        &self,
        src: &Source,
        raw_url: &str,
        expected_size: u64,
        expected_sha256: &str,
        max_bytes: usize,
        label: &str,
    ) -> Result<Vec<u8>, EngineError> {
        let u = file_url(raw_url, src).ok_or_else(|| {
            refused(422, "store_catalog_invalid", format!("Disallowed address for {label}."), "Report this item.")
        })?;
        let http = http_client(src)?;
        let cap = (expected_size as usize).min(max_bytes);
        let bytes = fetch_capped(&http, u, cap).await.map_err(|e| verified_error_map(e, "store_catalog_too_large"))?;
        let digest = hex_bytes(&sha2::Sha256::digest(&bytes));
        if bytes.len() as u64 != expected_size || !digest.eq_ignore_ascii_case(expected_sha256) {
            return Err(refused(
                502,
                "store_asset_tampered",
                format!("{label} failed the SHA-256 fingerprint check, so nothing was installed."),
                "The catalog file or asset changed after signing. Try again later.",
            ));
        }
        Ok(bytes)
    }

    /// `GET /api/v1/store/catalog?fresh=...`
    pub async fn store_catalog(&self, fresh: bool) -> Result<StoreCatalogView, EngineError> {
        let (verified, fetched_at, _) = self.fetch_verified_catalog(fresh).await?;
        let recs = self.db(repo::store_catalog::list_installs).await?;
        let last_undo = self.db(repo::store_catalog::latest_undo).await?;
        let mut by_id: HashMap<String, StoreInstallRecord> = HashMap::new();
        for r in &recs {
            by_id.insert(r.item_id.clone(), r.clone());
        }
        let items: Vec<StoreItem> = verified
            .items
            .iter()
            .map(|r| {
                let installed_version = by_id.get(&r.id).map(|i| i.version.clone());
                let update_available = installed_version.as_deref().is_some_and(|v| newer(&r.version, v));
                StoreItem {
                    id: r.id.clone(),
                    kind: r.kind,
                    title: r.title.clone(),
                    summary: r.summary.clone(),
                    version: r.version.clone(),
                    creator_id: r.creator_id.clone(),
                    tags: r.tags.clone(),
                    license: r.license.clone(),
                    sandboxed: r.sandboxed,
                    plugin_kind: r.plugin_kind.clone(),
                    plugin_actions: r.plugin_actions.clone(),
                    alert_events: r.alert_events.clone(),
                    featured: r.featured,
                    created_at: r.created_at,
                    updated_at: r.updated_at,
                    package: r.package.clone(),
                    assets: r.assets.clone(),
                    stats: r.stats.clone(),
                    preview: r.preview.clone(),
                    installed_version,
                    update_available,
                }
            })
            .collect();
        let installed = recs.iter().map(|r| installed_view(r, Some(&verified.items), last_undo.as_ref())).collect();
        let stale = now().0.saturating_sub(verified.generated_at) > STALE_AFTER_MS;
        Ok(StoreCatalogView {
            version: 1,
            generated_at: verified.generated_at,
            fetched_at,
            stale,
            creators: verified.creators,
            items,
            installed,
            last_undo,
            warnings: verified.warnings,
        })
    }

    /// `GET /api/v1/store/installed` — works cleanly even when catalog fetch fails or is offline.
    pub async fn store_installed(&self) -> Result<StoreInstalledView, EngineError> {
        let recs = self.db(repo::store_catalog::list_installs).await?;
        let last_undo = self.db(repo::store_catalog::latest_undo).await?;
        let cached = STORE_CACHE.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|c| c.catalog.items.clone());
        let installed = recs.iter().map(|r| installed_view(r, cached.as_deref(), last_undo.as_ref())).collect();
        Ok(StoreInstalledView { installed, last_undo })
    }

    /// `GET /api/v1/store/assets/{item_id}/{name}` — fetches & SHA-256 verifies a companion asset.
    pub async fn store_asset(&self, item_id: &str, name: &str) -> Result<(String, Vec<u8>), EngineError> {
        let (verified, _, src) = self.fetch_verified_catalog(false).await?;
        let item = verified.items.iter().find(|i| i.id == item_id).ok_or(EngineError::NotFound("store item"))?;
        let asset = item.assets.iter().find(|a| a.name == name).ok_or(EngineError::NotFound("store asset"))?;
        let bytes = self
            .fetch_verified_file(
                &src,
                &asset.url,
                asset.size,
                &asset.sha256,
                MAX_ASSET_BYTES,
                &format!("Asset '{}'", asset.name),
            )
            .await?;
        Ok((asset.mime.clone(), bytes))
    }

    /// `POST /api/v1/store/resolve-link` `{ uri }` — validates link and returns item preview without installing.
    pub async fn resolve_store_link(&self, uri: &str) -> Result<StoreLinkPreview, EngineError> {
        let parsed = parse_ferrocast_link(uri)?;
        let cat = self.store_catalog(false).await?;
        let item = cat.items.iter().find(|i| i.id == parsed.item_id).cloned().ok_or_else(|| {
            refused(
                404,
                "store_item_not_found",
                format!("Catalog doesn't have an item called \"{}\".", parsed.item_id),
                "Check that the link is current.",
            )
        })?;
        let creator = cat
            .creators
            .iter()
            .find(|c| c.id == item.creator_id)
            .cloned()
            .ok_or_else(|| EngineError::Internal("verified item missing creator".into()))?;
        let installed = cat.installed.iter().find(|r| r.item_id == item.id).cloned();
        let update_available = item.update_available;
        let mut safety_notes = vec![
            "Catalog signature verified (minisign).".to_string(),
            format!("Package size {} KB · SHA-256 checked on install.", (item.package.size / 1024).max(1)),
        ];
        match item.package.format {
            StorePackageFormat::Ferrodesign => {
                safety_notes.push("Pure data (.ferrodesign): contains no executable scripts or remote code.".into());
            }
            StorePackageFormat::Ferroalert => {
                safety_notes
                    .push("Custom alert HTML/CSS verified by p2e sandbox rules (isolated frame, no scripts).".into());
            }
            StorePackageFormat::Ferroplugin => {
                safety_notes.push(
                    "Sandboxed plugin verified (WGSL shader / resource-capped WASM / CSP no-network panel).".into(),
                );
            }
        }
        Ok(StoreLinkPreview {
            uri: parsed.uri,
            action: parsed.action,
            item,
            creator,
            installed,
            update_available,
            safety_notes,
            requires_confirmation: true,
        })
    }

    async fn install_verified_item(
        &self,
        item: &StoreItemRaw,
        creator: &StoreCreator,
        src: &Source,
        existing: Option<&StoreInstallRecord>,
        action: &str,
    ) -> Result<StoreMutationResult, EngineError> {
        // 1. Download & SHA-256 verify package + all declared assets BEFORE mutating any state.
        let max_pkg = max_package_size(item.package.format) as usize;
        let pkg_bytes = self
            .fetch_verified_file(src, &item.package.url, item.package.size, &item.package.sha256, max_pkg, &item.title)
            .await?;
        for asset in &item.assets {
            let _ = self
                .fetch_verified_file(
                    src,
                    &asset.url,
                    asset.size,
                    &asset.sha256,
                    MAX_ASSET_BYTES,
                    &format!("{} ({})", item.title, asset.name),
                )
                .await?;
        }

        // 2. Snapshot current state for one-click undo.
        let mut prev_designs = Vec::new();
        let mut prev_active_alert = None;
        let mut prev_rules = Vec::new();
        let mut prev_plugin = None;
        if let Some(old) = existing {
            prev_active_alert = self.active_alert_design().await?;
            for did in &old.design_ids {
                if let Ok(d) = self.design(did).await {
                    prev_designs.push(d);
                }
            }
            if !old.rule_ids.is_empty() {
                prev_rules = self.rules().into_iter().filter(|r| old.rule_ids.contains(&r.id)).collect();
            }
            if let Some(pid) = &old.plugin_id {
                let pinfo = self.plugins().await.into_iter().find(|p| &p.id == pid);
                let zip_b64 = snapshot_plugin_zip(&self.plugins_dir(), pid)?.unwrap_or_default();
                if let Some(p) = pinfo
                    && !zip_b64.is_empty()
                {
                    prev_plugin = Some(PluginSnapshot { id: p.id, zip_b64, on: p.on });
                }
            }
        }

        // 3. Validate and apply payload according to package format.
        let mut created_designs: Vec<Design> = Vec::new();
        let mut affected_rule_ids: Vec<String> = Vec::new();
        let mut installed_plugin_id: Option<String> = None;

        match item.package.format {
            StorePackageFormat::Ferrodesign => {
                let tmp = self.store().dir().join("tmp");
                let _ = std::fs::create_dir_all(&tmp);
                let path = tmp.join(format!("store-{}.ferrodesign", ferrocast_store::new_id()));
                std::fs::write(&path, &pkg_bytes).map_err(|e| EngineError::Internal(e.to_string()))?;
                let res = self.import_design_file(&path).await;
                let _ = std::fs::remove_file(&path);
                let report = res?;
                created_designs = report.designs;
            }
            StorePackageFormat::Ferroalert => {
                let bundle = check_alert_bundle(&pkg_bytes)?;
                // Build rule targets before mutating anything; initialize defaults if store has none.
                let mut current_rules = self.rules();
                if current_rules.is_empty() {
                    current_rules = default_rules();
                }
                let target_kinds: Vec<&'static str> = bundle.events.iter().map(|e| event_key_str(*e)).collect();
                let mut next_rules = Vec::new();
                for kind_name in target_kinds {
                    if let Some(mut r) = current_rules.iter().find(|r| r.kinds.iter().any(|k| k == kind_name)).cloned()
                    {
                        if !prev_rules.iter().any(|p| p.id == r.id) {
                            prev_rules.push(r.clone());
                        }
                        r.variations = merge_variations(&r.variations, &item.id, &bundle.variations);
                        r.validate().map_err(|e| EngineError::Invalid(e.to_string()))?;
                        affected_rule_ids.push(r.id.clone());
                        next_rules.push(r);
                    }
                }
                for r in next_rules {
                    self.save_rule(r).await?;
                }
            }
            StorePackageFormat::Ferroplugin => {
                let tmp = self.store().dir().join("tmp");
                let _ = std::fs::create_dir_all(&tmp);
                let path = tmp.join(format!("store-{}.zip", ferrocast_store::new_id()));
                std::fs::write(&path, &pkg_bytes).map_err(|e| EngineError::Internal(e.to_string()))?;
                let res = async {
                    let (manifest, _) = read_plugin_zip(&path)?;
                    if item.plugin_kind.as_deref() != Some(manifest.kind.as_str()) {
                        return Err(refused(
                            502,
                            "store_asset_tampered",
                            "The downloaded plugin's kind didn't match the catalog.",
                            "Nothing was installed.",
                        ));
                    }
                    if manifest.actions.iter().any(|a| !item.plugin_actions.contains(a)) {
                        return Err(refused(
                            502,
                            "store_asset_tampered",
                            "The downloaded plugin asked for permissions that weren't listed in the catalog.",
                            "Nothing was installed.",
                        ));
                    }
                    let info = self.install_plugin(&path).await?;
                    Ok::<String, EngineError>(info.id)
                }
                .await;
                let _ = std::fs::remove_file(&path);
                installed_plugin_id = Some(res?);
            }
        }

        // On update of a design package, remove old version's design IDs cleanly after new import succeeded.
        if action == "update"
            && let Some(old) = existing
        {
            for did in &old.design_ids {
                if self.active_alert_design().await?.as_deref() == Some(did.as_str()) {
                    let _ = self.set_active_alert_design(None).await;
                }
                let did_owned = did.clone();
                let _ = self.db(move |s| repo::designs::delete(s, &did_owned)).await;
            }
        }

        let created_design_ids: Vec<String> = created_designs.iter().map(|d| d.id.clone()).collect();
        let rec = StoreInstallRecord {
            item_id: item.id.clone(),
            kind: item.kind.key().into(),
            format: item.package.format.key().into(),
            title: item.title.clone(),
            version: item.version.clone(),
            creator_id: item.creator_id.clone(),
            creator_name: creator.name.clone(),
            package_sha256: item.package.sha256.clone(),
            design_ids: created_design_ids.clone(),
            rule_ids: affected_rule_ids.clone(),
            plugin_id: installed_plugin_id.clone(),
            installed_at: existing.map_or_else(|| now().0, |e| e.installed_at),
            updated_at: now().0,
        };
        let saved = self
            .db({
                let rec = rec.clone();
                move |s| repo::store_catalog::upsert_install(s, &rec)
            })
            .await?;

        let undo_entry = StoreUndoEntry {
            token: ferrocast_store::new_id(),
            action: action.into(),
            item_id: item.id.clone(),
            item_title: item.title.clone(),
            snapshot: StoreUndoSnapshot {
                prev_install: existing.cloned(),
                created_design_ids,
                prev_designs,
                prev_active_alert,
                prev_rules,
                created_plugin_id: installed_plugin_id,
                prev_plugin,
            },
            created_at: now().0,
        };
        let undo = self.db(move |s| repo::store_catalog::save_undo(s, &undo_entry)).await?;
        let installed = Some(installed_view(&saved, Some(std::slice::from_ref(item)), Some(&undo)));
        Ok(StoreMutationResult { action: action.into(), installed, designs: created_designs, undo })
    }

    /// `POST /api/v1/store/install` `{ id }`
    pub async fn install_store_item(&self, id: &str) -> Result<StoreMutationResult, EngineError> {
        if !good_store_id(id) {
            return Err(EngineError::Invalid("id: invalid store item id.".into()));
        }
        let existing = self
            .db({
                let id = id.to_string();
                move |s| repo::store_catalog::get_install(s, &id)
            })
            .await?;
        if existing.is_some() {
            return Err(EngineError::Conflict(
                "This store item is already installed. Use Update if a newer version is available.".into(),
            ));
        }
        let (verified, _, src) = self.fetch_verified_catalog(false).await?;
        let item = verified.items.iter().find(|i| i.id == id).ok_or_else(|| {
            refused(
                404,
                "store_item_not_found",
                format!("The catalog has no item called \"{id}\"."),
                "Refresh the Store and try again.",
            )
        })?;
        let creator = verified
            .creators
            .iter()
            .find(|c| c.id == item.creator_id)
            .ok_or_else(|| EngineError::Internal("verified item missing creator".into()))?;
        self.install_verified_item(item, creator, &src, None, "install").await
    }

    /// `POST /api/v1/store/update` `{ id }`
    pub async fn update_store_item(&self, id: &str) -> Result<StoreMutationResult, EngineError> {
        if !good_store_id(id) {
            return Err(EngineError::Invalid("id: invalid store item id.".into()));
        }
        let existing = self
            .db({
                let id = id.to_string();
                move |s| repo::store_catalog::get_install(s, &id)
            })
            .await?
            .ok_or_else(|| {
                refused(404, "store_item_not_found", "That store item isn't installed.", "Install it first.")
            })?;
        let (verified, _, src) = self.fetch_verified_catalog(false).await?;
        let item = verified.items.iter().find(|i| i.id == id).ok_or_else(|| {
            refused(
                404,
                "store_item_not_found",
                format!("The catalog no longer lists \"{id}\"."),
                "Check the catalog again.",
            )
        })?;
        if !newer(&item.version, &existing.version) {
            return Err(refused(
                409,
                "store_up_to_date",
                format!("{} is already at the latest version ({}).", item.title, existing.version),
                "Nothing to update.",
            ));
        }
        let creator = verified
            .creators
            .iter()
            .find(|c| c.id == item.creator_id)
            .ok_or_else(|| EngineError::Internal("verified item missing creator".into()))?;
        self.install_verified_item(item, creator, &src, Some(&existing), "update").await
    }

    /// `POST /api/v1/store/uninstall` `{ id }` — works cleanly even without catalog access.
    pub async fn uninstall_store_item(&self, id: &str) -> Result<StoreMutationResult, EngineError> {
        let existing = self
            .db({
                let id = id.to_string();
                move |s| repo::store_catalog::get_install(s, &id)
            })
            .await?
            .ok_or_else(|| {
                refused(404, "store_item_not_found", "That store item isn't installed.", "Reload Installed items.")
            })?;

        let prev_active_alert = self.active_alert_design().await?;
        let mut prev_designs = Vec::new();
        for did in &existing.design_ids {
            if let Ok(d) = self.design(did).await {
                prev_designs.push(d);
            }
            if prev_active_alert.as_ref() == Some(did) {
                let _ = self.set_active_alert_design(None).await;
            }
            let did_owned = did.clone();
            let _ = self.db(move |s| repo::designs::delete(s, &did_owned)).await;
        }

        let mut prev_rules = Vec::new();
        if !existing.rule_ids.is_empty() {
            let current = self.rules();
            let prefix = format!("store:{}:", existing.item_id);
            for mut r in current {
                if !existing.rule_ids.contains(&r.id) {
                    continue;
                }
                prev_rules.push(r.clone());
                let before = r.variations.len();
                r.variations.retain(|v| !v.id.starts_with(&prefix));
                if r.variations.len() != before {
                    if !r.variations.iter().any(|v| v.condition.is_none()) {
                        // Fall back cleanly to the built-in default look for this rule's primary kind.
                        let kind = r.kinds.first().map(String::as_str).unwrap_or("follow");
                        let fallback = default_rules()
                            .into_iter()
                            .find(|d| d.kinds.iter().any(|k| k == kind))
                            .and_then(|d| d.variations.into_iter().next())
                            .unwrap_or_else(|| Variation::new("default", "{name}"));
                        r.variations.insert(0, fallback);
                    }
                    let _ = self.save_rule(r).await;
                }
            }
        }

        let mut prev_plugin = None;
        if let Some(pid) = &existing.plugin_id {
            let pinfo = self.plugins().await.into_iter().find(|p| &p.id == pid);
            if let Some(zip_b64) = snapshot_plugin_zip(&self.plugins_dir(), pid)?
                && let Some(p) = pinfo
            {
                prev_plugin = Some(PluginSnapshot { id: p.id, zip_b64, on: p.on });
            }
            let _ = self.remove_plugin(pid).await;
        }

        let id_owned = existing.item_id.clone();
        self.db(move |s| repo::store_catalog::delete_install(s, &id_owned)).await?;

        let undo_entry = StoreUndoEntry {
            token: ferrocast_store::new_id(),
            action: "uninstall".into(),
            item_id: existing.item_id.clone(),
            item_title: existing.title.clone(),
            snapshot: StoreUndoSnapshot {
                prev_install: Some(existing.clone()),
                created_design_ids: Vec::new(),
                prev_designs,
                prev_active_alert,
                prev_rules,
                created_plugin_id: None,
                prev_plugin,
            },
            created_at: now().0,
        };
        let undo = self.db(move |s| repo::store_catalog::save_undo(s, &undo_entry)).await?;
        Ok(StoreMutationResult { action: "uninstall".into(), installed: None, designs: Vec::new(), undo })
    }

    /// `POST /api/v1/store/undo` `{ token? }` — one-click undo for install, update, or uninstall.
    pub async fn undo_store_action(&self, token: Option<&str>) -> Result<StoreUndoResult, EngineError> {
        let tok = token.map(str::to_string);
        let entry = self.db(move |s| repo::store_catalog::take_undo(s, tok.as_deref())).await?;
        let snap = &entry.snapshot;

        // Remove newly created designs from the undone install/update.
        for did in &snap.created_design_ids {
            if self.active_alert_design().await?.as_deref() == Some(did.as_str()) {
                let _ = self.set_active_alert_design(None).await;
            }
            let did_owned = did.clone();
            let _ = self.db(move |s| repo::designs::delete(s, &did_owned)).await;
        }
        // Restore previously snapshotted designs (for undo update or undo uninstall).
        for d in &snap.prev_designs {
            let d = d.clone();
            let _ = self.db(move |s| repo::designs::insert(s, &d)).await;
        }
        if snap.prev_active_alert.is_some() {
            let _ = self.set_active_alert_design(snap.prev_active_alert.as_deref()).await;
        }
        // Restore snapshotted alert rules.
        for r in &snap.prev_rules {
            self.save_rule(r.clone()).await?;
        }
        // Revert or restore plugins.
        if let Some(pid) = &snap.created_plugin_id
            && snap.prev_plugin.as_ref().is_none_or(|p| &p.id != pid)
        {
            let _ = self.remove_plugin(pid).await;
        }
        if let Some(p) = &snap.prev_plugin {
            let zip_bytes = base64::engine::general_purpose::STANDARD
                .decode(&p.zip_b64)
                .map_err(|e| EngineError::Internal(e.to_string()))?;
            let tmp = self.store().dir().join("tmp");
            let _ = std::fs::create_dir_all(&tmp);
            let path = tmp.join(format!("store-undo-{}.zip", ferrocast_store::new_id()));
            std::fs::write(&path, &zip_bytes).map_err(|e| EngineError::Internal(e.to_string()))?;
            let res = self.install_plugin(&path).await;
            let _ = std::fs::remove_file(&path);
            let _ = res?;
            if !p.on {
                let _ = self.set_plugin_on(&p.id, false).await;
            }
        }
        // Restore or remove store_installs row.
        match &snap.prev_install {
            Some(prev) => {
                let prev = prev.clone();
                let _ = self.db(move |s| repo::store_catalog::upsert_install(s, &prev)).await?;
            }
            None => {
                let id_owned = entry.item_id.clone();
                let _ = self.db(move |s| repo::store_catalog::delete_install(s, &id_owned)).await;
            }
        }
        let after = self.store_installed().await?;
        Ok(StoreUndoResult {
            reverted_action: entry.action,
            item_id: entry.item_id,
            item_title: entry.item_title,
            installed: after.installed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::response::Response;
    use axum::routing::get;
    use std::sync::{Arc, Mutex as StdMutex};

    const TEST_KEY: &str = include_str!("../testdata/store_catalog/test-key.pub");
    const CATALOG_V1: &[u8] = include_bytes!("../testdata/store_catalog/catalog.json");
    const CATALOG_V1_SIG: &str = include_str!("../testdata/store_catalog/catalog.json.sig");
    const CATALOG_V2: &[u8] = include_bytes!("../testdata/store_catalog/catalog-v2.json");
    const CATALOG_V2_SIG: &str = include_str!("../testdata/store_catalog/catalog-v2.json.sig");
    const MALICIOUS_CATALOG: &[u8] = include_bytes!("../testdata/store_catalog/malicious-catalog.json");
    const MALICIOUS_SIG: &str = include_str!("../testdata/store_catalog/malicious-catalog.json.sig");
    const PKG_THEME_V1: &[u8] = include_bytes!("../testdata/store_catalog/packages/aurora-pulse-1.0.0.ferrodesign");
    const PKG_THEME_V2: &[u8] = include_bytes!("../testdata/store_catalog/packages/aurora-pulse-1.1.0.ferrodesign");
    const PKG_ALERT: &[u8] = include_bytes!("../testdata/store_catalog/packages/arcade-ticker-1.0.0.ferroalert");
    const PKG_WIDGET: &[u8] = include_bytes!("../testdata/store_catalog/packages/foundry-goal-1.0.0.ferrodesign");
    const PKG_OVERLAY: &[u8] =
        include_bytes!("../testdata/store_catalog/packages/intermission-slate-1.0.0.ferrodesign");
    const PKG_PLUGIN: &[u8] = include_bytes!("../testdata/store_catalog/packages/soft-dimmer-1.0.0.zip");
    const ASSET_AURORA_PNG: &[u8] = include_bytes!("../testdata/store_catalog/assets/aurora-preview.png");
    const ASSET_ARCADE_PNG: &[u8] = include_bytes!("../testdata/store_catalog/assets/arcade-preview.png");

    static TEST_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[derive(Clone)]
    struct MockFixtureState {
        catalog: Vec<u8>,
        sig: String,
        files: HashMap<String, Vec<u8>>,
        fail_catalog: Option<StatusCode>,
    }

    impl Default for MockFixtureState {
        fn default() -> Self {
            let mut files = HashMap::new();
            files.insert("packages/aurora-pulse-1.0.0.ferrodesign".into(), PKG_THEME_V1.to_vec());
            files.insert("packages/aurora-pulse-1.1.0.ferrodesign".into(), PKG_THEME_V2.to_vec());
            files.insert("packages/arcade-ticker-1.0.0.ferroalert".into(), PKG_ALERT.to_vec());
            files.insert("packages/foundry-goal-1.0.0.ferrodesign".into(), PKG_WIDGET.to_vec());
            files.insert("packages/intermission-slate-1.0.0.ferrodesign".into(), PKG_OVERLAY.to_vec());
            files.insert("packages/soft-dimmer-1.0.0.zip".into(), PKG_PLUGIN.to_vec());
            files.insert("assets/aurora-preview.png".into(), ASSET_AURORA_PNG.to_vec());
            files.insert("assets/arcade-preview.png".into(), ASSET_ARCADE_PNG.to_vec());
            Self { catalog: CATALOG_V1.to_vec(), sig: CATALOG_V1_SIG.into(), files, fail_catalog: None }
        }
    }

    async fn spawn_fixture_server(state: Arc<StdMutex<MockFixtureState>>) -> u16 {
        let s1 = state.clone();
        let s2 = state.clone();
        let s3 = state.clone();
        let app = Router::new()
            .route(
                "/catalog.json",
                get(move || async move {
                    let st = s1.lock().unwrap().clone();
                    if let Some(code) = st.fail_catalog {
                        Response::builder().status(code).body(Body::empty()).unwrap()
                    } else {
                        Response::new(Body::from(st.catalog))
                    }
                }),
            )
            .route(
                "/catalog.json.sig",
                get(move || async move {
                    let st = s2.lock().unwrap().clone();
                    if let Some(code) = st.fail_catalog {
                        Response::builder().status(code).body(Body::empty()).unwrap()
                    } else {
                        Response::new(Body::from(st.sig))
                    }
                }),
            )
            .route(
                "/{*path}",
                get(move |axum::extract::Path(path): axum::extract::Path<String>| async move {
                    let st = s3.lock().unwrap().clone();
                    match st.files.get(&path) {
                        Some(b) => Response::new(Body::from(b.clone())),
                        None => Response::builder().status(StatusCode::NOT_FOUND).body(Body::empty()).unwrap(),
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await });
        port
    }

    #[tokio::test]
    async fn signature_failure_and_oversized_and_bad_schema_are_refused() {
        let _lock = TEST_GUARD.lock().await;
        let state = Arc::new(StdMutex::new(MockFixtureState::default()));
        let port = spawn_fixture_server(state.clone()).await;
        let (engine, _dir, _studio, _chat) = crate::automation::tests::engine().await;

        assert_ne!(STORE_CATALOG_KEY.trim(), TEST_KEY.trim());
        set_source(Source { url: format!("http://127.0.0.1:{port}/catalog.json"), key: TEST_KEY.into(), local: true });

        // 1. Valid signature succeeds.
        let cat = engine.store_catalog(true).await.unwrap();
        assert_eq!(cat.items.len(), 5);
        assert_eq!(cat.creators.len(), 2);

        // 2. Tampered catalog byte -> signature failure (502 store_bad_signature).
        state.lock().unwrap().catalog[25] ^= 1;
        let bad_sig = engine.store_catalog(true).await;
        assert!(matches!(bad_sig, Err(EngineError::Refused { code: "store_bad_signature", .. })), "{bad_sig:?}");

        // 3. Wrong .sig text -> signature failure.
        state.lock().unwrap().catalog = CATALOG_V1.to_vec();
        state.lock().unwrap().sig = "not a signature".into();
        let bad_sig2 = engine.store_catalog(true).await;
        assert!(matches!(bad_sig2, Err(EngineError::Refused { code: "store_bad_signature", .. })), "{bad_sig2:?}");

        // 4. Oversized catalog -> refused before parsing/signature (413 store_catalog_too_large).
        state.lock().unwrap().catalog = vec![b' '; MAX_CATALOG_BYTES + 1024];
        state.lock().unwrap().sig = CATALOG_V1_SIG.into();
        let too_big = engine.store_catalog(true).await;
        assert!(
            matches!(too_big, Err(EngineError::Refused { status: 413, code: "store_catalog_too_large", .. })),
            "{too_big:?}"
        );
    }

    #[tokio::test]
    async fn malicious_item_fields_and_unsafe_html_are_blocked() {
        let _lock = TEST_GUARD.lock().await;
        let state = Arc::new(StdMutex::new(MockFixtureState::default()));
        let port = spawn_fixture_server(state.clone()).await;
        let (engine, _dir, _studio, _chat) = crate::automation::tests::engine().await;
        set_source(Source { url: format!("http://127.0.0.1:{port}/catalog.json"), key: TEST_KEY.into(), local: true });

        // Validly signed catalog containing traversal, SSRF localhost URL, HTML XSS title, and malicious creator.
        {
            let mut st = state.lock().unwrap();
            st.catalog = MALICIOUS_CATALOG.to_vec();
            st.sig = MALICIOUS_SIG.into();
        }
        let cat = engine.store_catalog(true).await.unwrap();
        assert!(cat.items.is_empty(), "all malicious items must be blocked: {:?}", cat.items);
        assert_eq!(cat.creators.len(), 1, "bad-creator must be filtered");
        assert!(cat.warnings.len() >= 4, "all malicious reasons surfaced: {:?}", cat.warnings);

        // Even directly validating a custom alert bundle containing <script> fails p2e sandbox check.
        let evil_alert = serde_json::json!({
            "schema": 1,
            "id": "evil.alert",
            "name": "Evil Alert",
            "events": ["follow"],
            "variations": [{
                "id": "v1",
                "name": "v1",
                "condition": null,
                "priority": 0,
                "weight": 1,
                "title": "{name}",
                "body": "{message}",
                "image": null,
                "sound": null,
                "volume": 70,
                "duration_ms": 5000,
                "tts": false,
                "layout": "stack",
                "css": "",
                "anim": { "in": "fade", "out": "fade", "text": "none", "in_ms": 300, "out_ms": 300, "text_delay_ms": 100 },
                "custom_enabled": true,
                "html": "<div onclick=\"fetch('https://evil.test')\"><script>alert(1)</script></div>",
                "fields": [],
                "field_values": {}
            }]
        })
        .to_string();
        let blocked = check_alert_bundle(evil_alert.as_bytes());
        assert!(matches!(blocked, Err(EngineError::Refused { code: "store_unsafe_package", .. })), "{blocked:?}");
    }

    #[tokio::test]
    async fn tampered_asset_hash_blocks_install_and_asset_fetch() {
        let _lock = TEST_GUARD.lock().await;
        let state = Arc::new(StdMutex::new(MockFixtureState::default()));
        let port = spawn_fixture_server(state.clone()).await;
        let (engine, _dir, _studio, _chat) = crate::automation::tests::engine().await;
        set_source(Source { url: format!("http://127.0.0.1:{port}/catalog.json"), key: TEST_KEY.into(), local: true });

        // Swap one byte of preview asset -> store_asset & install both reject!
        {
            let mut st = state.lock().unwrap();
            let png = st.files.get_mut("assets/aurora-preview.png").unwrap();
            png[8] ^= 0xff;
        }
        let asset_err = engine.store_asset("aurora-pulse", "preview.png").await;
        assert!(matches!(asset_err, Err(EngineError::Refused { code: "store_asset_tampered", .. })), "{asset_err:?}");
        let pkg_err = engine.install_store_item("aurora-pulse").await;
        assert!(matches!(pkg_err, Err(EngineError::Refused { code: "store_asset_tampered", .. })), "{pkg_err:?}");
        assert!(engine.designs(None).await.unwrap().is_empty());

        // Restore asset, tamper package zip byte -> rejected cleanly.
        {
            let mut st = state.lock().unwrap();
            st.files.insert("assets/aurora-preview.png".into(), ASSET_AURORA_PNG.to_vec());
            st.files.get_mut("packages/aurora-pulse-1.0.0.ferrodesign").unwrap()[10] ^= 1;
        }
        let pkg_err2 = engine.install_store_item("aurora-pulse").await;
        assert!(matches!(pkg_err2, Err(EngineError::Refused { code: "store_asset_tampered", .. })), "{pkg_err2:?}");
        assert!(engine.store_installed().await.unwrap().installed.is_empty());
    }

    #[tokio::test]
    async fn install_update_uninstall_undo_and_stale_catalog_resilience() {
        let _lock = TEST_GUARD.lock().await;
        let state = Arc::new(StdMutex::new(MockFixtureState::default()));
        let port = spawn_fixture_server(state.clone()).await;
        let (engine, _dir, _studio, _chat) = crate::automation::tests::engine().await;
        set_source(Source { url: format!("http://127.0.0.1:{port}/catalog.json"), key: TEST_KEY.into(), local: true });

        // Install .ferrodesign theme -> creates designs & records install.
        let installed = engine.install_store_item("aurora-pulse").await.unwrap();
        assert_eq!(installed.action, "install");
        assert_eq!(installed.designs.len(), 2);
        assert_eq!(engine.designs(None).await.unwrap().len(), 2);

        // One-click Undo install -> removes created designs and install row.
        let undone = engine.undo_store_action(Some(&installed.undo.token)).await.unwrap();
        assert_eq!(undone.reverted_action, "install");
        assert!(engine.designs(None).await.unwrap().is_empty());
        assert!(engine.store_installed().await.unwrap().installed.is_empty());

        // Install again, plus custom HTML sandboxed alert & shader plugin.
        engine.install_store_item("aurora-pulse").await.unwrap();
        let custom = engine.install_store_item("arcade-ticker-alert").await.unwrap();
        assert_eq!(custom.installed.as_ref().unwrap().rule_ids.len(), 1);
        let follow_rule = engine.rules().into_iter().find(|r| r.kinds.contains(&"follow".to_string())).unwrap();
        assert!(
            follow_rule.variations.iter().any(|v| v.id.starts_with("store:arcade-ticker-alert:") && v.custom_enabled)
        );

        let plug = engine.install_store_item("soft-dimmer-shader").await.unwrap();
        assert_eq!(plug.installed.as_ref().unwrap().plugin_id.as_deref(), Some("dev.kira.softvignette"));
        assert_eq!(engine.plugins().await.len(), 1);

        // Switch catalog to v2 and update Aurora Pulse 1.0.0 -> 1.1.0, then Undo update -> back to 1.0.0.
        {
            let mut st = state.lock().unwrap();
            st.catalog = CATALOG_V2.to_vec();
            st.sig = CATALOG_V2_SIG.into();
        }
        let cat_v2 = engine.store_catalog(true).await.unwrap();
        assert!(cat_v2.items.iter().find(|i| i.id == "aurora-pulse").unwrap().update_available);
        let upd = engine.update_store_item("aurora-pulse").await.unwrap();
        assert_eq!(upd.installed.as_ref().unwrap().version, "1.1.0");
        engine.undo_store_action(Some(&upd.undo.token)).await.unwrap();
        let after_undo = engine.store_installed().await.unwrap();
        assert_eq!(after_undo.installed.iter().find(|i| i.item_id == "aurora-pulse").unwrap().version, "1.0.0");

        // Break upstream catalog completely (HTTP 500 -> store_unreachable): installed items, designs,
        // plugin, uninstall AND undo all still work cleanly!
        state.lock().unwrap().fail_catalog = Some(StatusCode::INTERNAL_SERVER_ERROR);
        assert!(matches!(
            engine.store_catalog(true).await,
            Err(EngineError::Refused { code: "store_unreachable", .. })
        ));
        let offline_list = engine.store_installed().await.unwrap();
        assert_eq!(offline_list.installed.len(), 3);
        let un = engine.uninstall_store_item("soft-dimmer-shader").await.unwrap();
        assert!(engine.plugins().await.is_empty());
        engine.undo_store_action(Some(&un.undo.token)).await.unwrap();
        assert_eq!(engine.plugins().await.len(), 1);

        // ferrocast:// links validate strictly and NEVER install automatically.
        state.lock().unwrap().fail_catalog = None;
        let bad = parse_ferrocast_link("ferrocast://store/install/../evil");
        assert!(bad.is_err());
        let bad_scheme = parse_ferrocast_link("https://evil.com/store/install/aurora-pulse");
        assert!(bad_scheme.is_err());
        let preview = engine.resolve_store_link("ferrocast://store/install/foundry-goal-widget").await.unwrap();
        assert!(preview.requires_confirmation);
        assert!(preview.installed.is_none());
    }
}
