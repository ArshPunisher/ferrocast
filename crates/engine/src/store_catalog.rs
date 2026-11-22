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
    DisplacedDefault, PluginSnapshot, RuleChange, StoreInstallRecord, StoreUndoEntry, StoreUndoSnapshot,
    StoreUndoSummary,
};
use ferrocast_store::{designfile, repo};
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::plugin_catalog::{Source, client as http_client, fetch_capped, file_url, hex_bytes, newer};
use crate::plugins::{ACTIONS as PLUGIN_ACTIONS, MAX_ZIP as MAX_PLUGIN_ZIP, read_zip as read_plugin_zip};
use crate::{Engine, EngineError, now};

/// Default signed Community Store catalog endpoint (companion `.sig` lives beside it).
pub const STORE_CATALOG_URL: &str =
    "https://raw.githubusercontent.com/ArshPunisher/ferrocast-store/main/store-catalog.json";

/// The file name the Store catalog is signed under (the signature's trusted comment says it): a
/// plugin catalog signature made with the same key can't pass as a Store catalog.
pub const STORE_CATALOG_FILE: &str = "store-catalog.json";

/// The catalog's highest accepted signing time and `generated_at` (settings key): an older
/// catalog than this is a replay and is refused.
const FLOOR_KEY: &str = "store_catalog_floor";

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
struct CatalogFloor {
    signed_at: i64,
    generated_at: i64,
}

/// Checks the Store catalog: signature, and that it was signed as the Store catalog.
pub fn verify_store(list: &[u8], sig: &str, key: &str) -> Result<crate::plugin_catalog::SignedFor, String> {
    let s = crate::plugin_catalog::verify_signed(list, sig, key)?;
    if s.file != STORE_CATALOG_FILE {
        return Err("the signature is for another catalog".into());
    }
    Ok(s)
}

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

#[cfg(any(test, feature = "test-catalog"))]
static STORE_SOURCE: Mutex<Option<Source>> = Mutex::new(None);
static STORE_CACHE: Mutex<Option<CachedCatalog>> = Mutex::new(None);

/// Install, update, uninstall and undo take turns: two at once could both pass the "already
/// installed?" check and import everything twice.
static STORE_WRITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Accepting a catalog (floor check, floor update, cache) takes turns too.
static STORE_PUBLISH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct SignedAt {
    timestamp: i64,
}

/// Tests only: holds the next catalog being accepted inside its turn until released.
#[cfg(test)]
static PUBLISH_PAUSE: Mutex<Option<std::sync::Arc<tokio::sync::Notify>>> = Mutex::new(None);

#[cfg(test)]
async fn test_pause_in_publish() {
    let hold = PUBLISH_PAUSE.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(n) = hold {
        n.notified().await;
    }
}

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

/// Overrides the Community Store catalog URL/key. Tests only: this function does not exist in a
/// release build (the `test-catalog` feature is for the server's tests).
#[cfg(any(test, feature = "test-catalog"))]
pub fn set_source(s: Source) {
    *STORE_SOURCE.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
    *STORE_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Clears any test override for the Community Store catalog.
#[cfg(any(test, feature = "test-catalog"))]
pub fn clear_source() {
    *STORE_SOURCE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *STORE_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn current_source() -> Source {
    #[cfg(any(test, feature = "test-catalog"))]
    if let Some(s) = STORE_SOURCE.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return s;
    }
    // A release build has only this: the pinned key, https.
    Source { url: STORE_CATALOG_URL.into(), key: STORE_CATALOG_KEY.into(), local: false }
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
    /// The id the plugin's own manifest must carry (a Store plugin item names it).
    #[serde(default)]
    pub plugin_id: Option<String>,
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
    // The one grammar the update comparison reads: a version admitted here is always comparable.
    let ok = (1..=32).contains(&v.len()) && crate::plugin_catalog::readable_version(v);
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
    let private_v4 = |v4: std::net::Ipv4Addr| {
        let o = v4.octets();
        v4.is_loopback()
            || v4.is_private()
            || v4.is_link_local()
            || v4.is_unspecified()
            || v4.is_broadcast()
            || (o[0] == 100 && (64..128).contains(&o[1]))
    };
    // An IPv6 literal comes back in brackets: the address is what is inside.
    let bare = host.trim_start_matches('[').trim_end_matches(']').trim_end_matches('.').to_ascii_lowercase();
    let private = match bare.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => private_v4(v4),
        Ok(std::net::IpAddr::V6(v6)) => {
            let seg = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || (seg & 0xfe00) == 0xfc00 // unique local
                || (seg & 0xffc0) == 0xfe80 // link-local
                || v6.to_ipv4_mapped().is_some_and(private_v4)
        }
        Err(_) => bare == "localhost" || bare.ends_with(".localhost") || bare.ends_with(".local"),
    };
    if private {
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

/// Whether a file's first bytes are what its declared type says (a catalog's word is not enough
/// for a file that is served to the page).
fn asset_matches_mime(mime: &str, b: &[u8]) -> bool {
    match mime {
        "image/png" => b.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
        "image/jpeg" => b.starts_with(&[0xFF, 0xD8, 0xFF]),
        "image/gif" => b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a"),
        "image/webp" => b.len() > 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP",
        "audio/mpeg" => b.starts_with(b"ID3") || (b.len() > 2 && b[0] == 0xFF && (b[1] & 0xE0) == 0xE0),
        "audio/ogg" => b.starts_with(b"OggS"),
        "audio/wav" => b.len() > 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WAVE",
        _ => false,
    }
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
        match item.plugin_id.as_deref() {
            Some(id) if crate::plugins::good_id(id) => {}
            _ => return Err("plugin_id: the plugin's own id (like dev.you.name) is required".into()),
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
    if u.port().is_some() || u.fragment().is_some() {
        return Err(EngineError::Invalid("uri: no port or fragment in a Ferrocast link.".into()));
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

/// What putting a pack's variations into a rule does.
struct Merged {
    variations: Vec<Variation>,
    /// Taken out: the pack's own earlier variations (an update) and the user's default if the
    /// pack brings one.
    removed: Vec<Variation>,
    /// The user's own default look that was taken out (not one of the pack's).
    displaced: Option<Variation>,
    /// The ids the pack's variations have in the rule.
    added: Vec<String>,
}

fn merge_pack(existing: &[Variation], item_id: &str, incoming: &[Variation]) -> Merged {
    let prefix = format!("store:{item_id}:");
    let mut removed: Vec<Variation> = existing.iter().filter(|v| v.id.starts_with(&prefix)).cloned().collect();
    let mut out: Vec<Variation> = existing.iter().filter(|v| !v.id.starts_with(&prefix)).cloned().collect();
    // If the incoming pack supplies a default variation (condition: None), it takes the rule's
    // default place: the one there goes (and is kept, so removing the pack gives it back).
    let mut displaced = None;
    if incoming.iter().any(|v| v.condition.is_none()) {
        displaced = out.iter().find(|v| v.condition.is_none()).cloned();
        out.retain(|v| v.condition.is_some());
        removed.extend(displaced.clone());
    }
    let mut added = Vec::new();
    for v in incoming {
        let mut tagged = v.clone();
        tagged.id = format!("{prefix}{}", v.id);
        added.push(tagged.id.clone());
        out.push(tagged);
    }
    Merged { variations: out, removed, displaced, added }
}

/// What an install changes, as it changes it.
struct Applied {
    snap: StoreUndoSnapshot,
    displaced: Vec<DisplacedDefault>,
    designs: Vec<Design>,
    rule_ids: Vec<String>,
    plugin_id: Option<String>,
}

/// Why a downloaded plugin package isn't the one the catalog lists: its own id differs.
/// What a link's confirmation says about safety. Only what is true before anything is
/// downloaded is said as done (the catalog's signature); what happens to the package is said as
/// what will be checked when the person installs it.
fn preview_notes(format: StorePackageFormat, size: u64) -> Vec<String> {
    let mut notes = vec![
        "The catalog's signature was checked (minisign).".to_string(),
        format!("Package size {} KB. Its SHA-256 is checked when you install.", (size / 1024).max(1)),
    ];
    notes.push(match format {
        StorePackageFormat::Ferrodesign => {
            "A .ferrodesign is pure data. It is opened and checked when you install; nothing runs from it.".into()
        }
        StorePackageFormat::Ferroalert => {
            "When you install, the alert's HTML and CSS are checked against the sandbox rules (an isolated frame, no scripts). Nothing is applied if a check fails.".into()
        }
        StorePackageFormat::Ferroplugin => {
            "When you install, the plugin is opened and its kind, id and permissions are compared with the catalog's. It runs sandboxed (a WGSL shader, resource-capped WASM, or a panel with no network).".into()
        }
    });
    notes
}

fn plugin_identity_problem(expected: &str, manifest_id: &str) -> Option<String> {
    (!crate::plugins::good_id(manifest_id) || manifest_id != expected)
        .then(|| format!("The plugin's own id ({manifest_id}) isn't the one the catalog lists ({expected})."))
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
        let signed = match verify_store(&list, &sig, &src.key) {
            Ok(s) => s,
            Err(why) => {
                tracing::warn!(%why, "community store catalog failed minisign check");
                return Err(refused(
                    502,
                    "store_bad_signature",
                    "The Community Store catalog signature isn't valid, so it wasn't opened.",
                    "Installed items stay safe and working. Try refreshing later.",
                ));
            }
        };
        let verified = parse_and_validate_catalog(&list, &src)?;
        let (verified, fetched_at) = self.publish_catalog(signed.timestamp, verified).await?;
        Ok((verified, fetched_at, src))
    }

    /// Accepts a verified catalog: checks it against the floor (nothing older than what was
    /// accepted before), raises the floor and puts it in the cache. One at a time, so that two
    /// refreshes finishing in the other order than they started can't leave the older one's
    /// catalog in the cache, or let it pass a floor the newer one is about to raise.
    async fn publish_catalog(
        &self,
        signed_at: i64,
        verified: VerifiedCatalog,
    ) -> Result<(VerifiedCatalog, i64), EngineError> {
        let _in_turn = STORE_PUBLISH.lock().await;
        // Nothing older than what was accepted before: a replayed old (signed) catalog would bring
        // back an item that was taken down.
        let floor: CatalogFloor = self.db(|s| repo::settings::get_or_default(s, FLOOR_KEY)).await?;
        #[cfg(test)]
        test_pause_in_publish().await;
        let signed = SignedAt { timestamp: signed_at };
        if signed.timestamp < floor.signed_at || verified.generated_at < floor.generated_at {
            tracing::warn!(
                signed = signed.timestamp,
                floor = floor.signed_at,
                "community store catalog is older than the last accepted one"
            );
            return Err(refused(
                502,
                "store_catalog_rollback",
                "The Community Store catalog is older than the one Ferrocast already accepted, so it wasn't opened.",
                "This can be an old copy served by mistake or on purpose. Try again later.",
            ));
        }
        let next = CatalogFloor {
            signed_at: signed.timestamp.max(floor.signed_at),
            generated_at: verified.generated_at.max(floor.generated_at),
        };
        if next.signed_at != floor.signed_at || next.generated_at != floor.generated_at {
            self.db(move |s| repo::settings::set(s, FLOOR_KEY, &next)).await?;
        }
        let fetched_at = now().0;
        *STORE_CACHE.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(CachedCatalog { at: Instant::now(), fetched_at, catalog: verified.clone() });
        Ok((verified, fetched_at))
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
        if !asset_matches_mime(&asset.mime, &bytes) {
            return Err(refused(
                502,
                "store_asset_tampered",
                format!("Asset '{}' isn't the kind of file the catalog says it is.", asset.name),
                "Nothing was shown. Report this item.",
            ));
        }
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
        // A link for one version doesn't quietly offer another.
        if let Some(v) = &parsed.version
            && *v != item.version
        {
            return Err(refused(
                409,
                "store_version_mismatch",
                format!("The link is for version {v}, and the Store has {} of \"{}\".", item.version, item.title),
                "Ask for a current link, or open the item in the Store.",
            ));
        }
        let creator = cat
            .creators
            .iter()
            .find(|c| c.id == item.creator_id)
            .cloned()
            .ok_or_else(|| EngineError::Internal("verified item missing creator".into()))?;
        let installed = cat.installed.iter().find(|r| r.item_id == item.id).cloned();
        let update_available = item.update_available;
        let safety_notes = preview_notes(item.package.format, item.package.size);
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

    /// What a package changes, as it goes: every change is written down in `snap` the moment it
    /// is made, so a failure part way (or a failed commit) can be taken back exactly.
    async fn apply_item(
        &self,
        item: &StoreItemRaw,
        pkg_bytes: &[u8],
        existing: Option<&StoreInstallRecord>,
        action: &str,
        applied: &mut Applied,
    ) -> Result<(), EngineError> {
        match item.package.format {
            StorePackageFormat::Ferrodesign => {
                let tmp = self.store().dir().join("tmp");
                let _ = std::fs::create_dir_all(&tmp);
                let path = tmp.join(format!("store-{}.ferrodesign", ferrocast_store::new_id()));
                std::fs::write(&path, pkg_bytes).map_err(|e| EngineError::Internal(e.to_string()))?;
                let res = self.import_design_file(&path).await;
                let _ = std::fs::remove_file(&path);
                let report = res?;
                applied.snap.created_design_ids = report.designs.iter().map(|d| d.id.clone()).collect();
                applied.designs = report.designs;
            }
            StorePackageFormat::Ferroalert => {
                let bundle = check_alert_bundle(pkg_bytes)?;
                // Plan every rule before changing one: the packs's variations go into each rule that
                // takes its events, and nothing is touched if there is no such rule.
                let (mut current_rules, from_defaults) = match self.rules() {
                    r if r.is_empty() => (default_rules(), true),
                    r => (r, false),
                };
                let mut plan: Vec<(ferrocast_core::alert::AlertRule, RuleChange)> = Vec::new();
                for kind_name in bundle.events.iter().map(|e| event_key_str(*e)) {
                    let Some(rule) = current_rules.iter_mut().find(|r| r.kinds.iter().any(|k| k == kind_name)) else {
                        continue;
                    };
                    if plan.iter().any(|(r, _)| r.id == rule.id) {
                        continue;
                    }
                    let carried = existing.and_then(|o| o.displaced.iter().find(|d| d.rule_id == rule.id));
                    let merged = merge_pack(&rule.variations, &item.id, &bundle.variations);
                    let mut next = rule.clone();
                    next.variations = merged.variations;
                    next.validate().map_err(|e| EngineError::Invalid(e.to_string()))?;
                    // The user's own default, taken out by this pack's: kept with the install (the
                    // one displaced first stays across updates, which find the pack's own there).
                    if let Some(v) = merged.displaced
                        && carried.is_none()
                    {
                        applied.displaced.push(DisplacedDefault { rule_id: rule.id.clone(), variation: v });
                    }
                    plan.push((
                        next,
                        RuleChange {
                            rule_id: rule.id.clone(),
                            removed: merged.removed,
                            added: merged.added,
                            created: from_defaults,
                        },
                    ));
                }
                if plan.is_empty() {
                    let wanted: Vec<&str> = bundle.events.iter().map(|e| event_key_str(*e)).collect();
                    return Err(refused(
                        422,
                        "store_alert_no_target",
                        format!("You have no alert for {} for this pack to go into.", wanted.join(", ")),
                        "Add that alert in Alerts first, then install the pack. Nothing was changed.",
                    ));
                }
                for (rule, change) in plan {
                    self.save_rule(rule.clone()).await?;
                    applied.rule_ids.push(rule.id);
                    applied.snap.rule_changes.push(change);
                }
            }
            StorePackageFormat::Ferroplugin => {
                let tmp = self.store().dir().join("tmp");
                let _ = std::fs::create_dir_all(&tmp);
                let path = tmp.join(format!("store-{}.zip", ferrocast_store::new_id()));
                std::fs::write(&path, pkg_bytes).map_err(|e| EngineError::Internal(e.to_string()))?;
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
                    let expected = item.plugin_id.as_deref().unwrap_or_default();
                    if let Some(why) = plugin_identity_problem(expected, &manifest.id) {
                        return Err(refused(502, "store_asset_tampered", why, "Nothing was installed."));
                    }
                    // The Store never overwrites a plugin it didn't install: the user's own copy
                    // stays as it is (and an Undo can't delete it).
                    let owned = existing.and_then(|o| o.plugin_id.as_deref()) == Some(manifest.id.as_str());
                    if !owned && self.plugins().await.iter().any(|p| p.id == manifest.id) {
                        return Err(refused(
                            409,
                            "store_plugin_exists",
                            format!("A plugin with the id {} is already installed.", manifest.id),
                            "Remove it first in Plugins if you want the Store's copy. Nothing was changed.",
                        ));
                    }
                    let info = self.install_plugin(&path).await?;
                    Ok::<String, EngineError>(info.id)
                }
                .await;
                let _ = std::fs::remove_file(&path);
                let id = res?;
                applied.snap.created_plugin_id = Some(id.clone());
                applied.plugin_id = Some(id);
            }
        }

        // On update of a design package, the old version's designs go once the new ones are in.
        if action == "update"
            && let Some(old) = existing
        {
            for did in &old.design_ids {
                if self.active_alert_design().await?.as_deref() == Some(did.as_str()) {
                    self.set_active_alert_design(None).await?;
                    applied.snap.cleared_active_alert = Some(did.clone());
                }
                self.delete_design_row(did).await?;
            }
        }
        Ok(())
    }

    /// Removes a design row; one that is already gone is fine (a retry may have removed it).
    async fn delete_design_row(&self, id: &str) -> Result<(), EngineError> {
        let id = id.to_string();
        match self.db(move |s| repo::designs::delete(s, &id)).await {
            Err(EngineError::Store(ferrocast_store::StoreError::NotFound(_))) => Ok(()),
            other => other,
        }
    }

    /// Puts back what a Store action changed, using only what the action itself recorded:
    /// the designs and plugin it made go, the ones it removed come back, the alert variations it
    /// added or took out are reversed in the rule as it is now (a later edit stays), the active
    /// alert is restored only if nothing was picked since, and the install row is as it was.
    /// Every step is tried and may be repeated; the first failure is reported after the rest ran.
    async fn restore_snapshot(&self, item_id: &str, snap: &StoreUndoSnapshot) -> Result<(), EngineError> {
        let mut first_error: Option<EngineError> = None;
        let mut note = |r: Result<(), EngineError>| {
            if let Err(e) = r {
                tracing::warn!(error = %e, item = item_id, "a Store restore step failed");
                first_error.get_or_insert(e);
            }
        };
        for did in &snap.created_design_ids {
            let r = async {
                if self.active_alert_design().await?.as_deref() == Some(did.as_str()) {
                    self.set_active_alert_design(None).await?;
                }
                self.delete_design_row(did).await
            }
            .await;
            note(r);
        }
        for d in &snap.prev_designs {
            // Already back (an earlier try got this far): nothing to do.
            if self.design(&d.id).await.is_ok() {
                continue;
            }
            let d = d.clone();
            note(self.db(move |s| repo::designs::insert(s, &d)).await.map(|_| ()));
        }
        if let Some(id) = &snap.cleared_active_alert {
            let r = async {
                if self.active_alert_design().await?.is_none() && self.design(id).await.is_ok() {
                    self.set_active_alert_design(Some(id)).await?;
                }
                Ok(())
            }
            .await;
            note(r);
        }
        note(self.reverse_rule_changes(&snap.rule_changes).await);
        // The plugin the action installed goes unless it replaced one that comes back.
        if let Some(pid) = &snap.created_plugin_id
            && snap.prev_plugin.as_ref().is_none_or(|p| &p.id != pid)
        {
            let r = match self.remove_plugin(pid).await {
                Err(EngineError::NotFound(_)) => Ok(()),
                other => other,
            };
            note(r);
        }
        if let Some(p) = &snap.prev_plugin {
            let r = async {
                let zip_bytes = base64::engine::general_purpose::STANDARD
                    .decode(&p.zip_b64)
                    .map_err(|e| EngineError::Internal(e.to_string()))?;
                let tmp = self.store().dir().join("tmp");
                let _ = std::fs::create_dir_all(&tmp);
                let path = tmp.join(format!("store-undo-{}.zip", ferrocast_store::new_id()));
                std::fs::write(&path, &zip_bytes).map_err(|e| EngineError::Internal(e.to_string()))?;
                let res = self.install_plugin(&path).await;
                let _ = std::fs::remove_file(&path);
                res?;
                if !p.on {
                    self.set_plugin_on(&p.id, false).await?;
                }
                Ok(())
            }
            .await;
            note(r);
        }
        let row = match &snap.prev_install {
            Some(prev) => {
                let prev = prev.clone();
                self.db(move |s| repo::store_catalog::upsert_install(s, &prev)).await.map(|_| ())
            }
            None => {
                let id = item_id.to_string();
                match self.db(move |s| repo::store_catalog::delete_install(s, &id)).await {
                    Err(EngineError::Store(ferrocast_store::StoreError::NotFound(_))) => Ok(()),
                    other => other,
                }
            }
        };
        note(row);
        first_error.map_or(Ok(()), Err)
    }

    /// The inverse of what a Store action did to alert rules, applied to each rule as it is now.
    async fn reverse_rule_changes(&self, changes: &[RuleChange]) -> Result<(), EngineError> {
        let mut first_error: Option<EngineError> = None;
        for c in changes {
            let Some(mut rule) = self.rules().into_iter().find(|r| r.id == c.rule_id) else {
                // The person deleted the rule since: there is nothing of ours left in it.
                continue;
            };
            let before = rule.variations.clone();
            rule.variations.retain(|v| !c.added.contains(&v.id));
            for v in &c.removed {
                if rule.variations.iter().any(|x| x.id == v.id) {
                    continue;
                }
                // A rule keeps one default: a taken-out default comes back only if none took its place.
                if v.condition.is_none() {
                    if rule.variations.iter().any(|x| x.condition.is_none()) {
                        continue;
                    }
                    rule.variations.insert(0, v.clone());
                } else {
                    rule.variations.push(v.clone());
                }
            }
            // A rule the Store made (there were none) that is back to the single look it made goes too.
            let r = if c.created
                && rule.variations.len() == 1
                && rule.variations.iter().all(|v| v.id.ends_with("-default"))
            {
                self.delete_rule(&rule.id).await
            } else if rule.variations != before {
                self.save_rule(rule).await.map(|_| ())
            } else {
                Ok(())
            };
            if let Err(e) = r {
                first_error.get_or_insert(e);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    async fn install_verified_item(
        &self,
        item: &StoreItemRaw,
        creator: &StoreCreator,
        src: &Source,
        existing: Option<&StoreInstallRecord>,
        action: &str,
    ) -> Result<StoreMutationResult, EngineError> {
        // 1. Download & SHA-256 verify the package BEFORE mutating any state.
        let max_pkg = max_package_size(item.package.format) as usize;
        let pkg_bytes = self
            .fetch_verified_file(src, &item.package.url, item.package.size, &item.package.sha256, max_pkg, &item.title)
            .await?;
        // Preview assets aren't installed: each is fetched and fingerprint-checked when shown
        // (`store_asset`), so an install doesn't download them.

        // 2. What an update will replace, kept for one-click undo.
        let mut applied = Applied {
            snap: StoreUndoSnapshot {
                prev_install: existing.cloned(),
                created_design_ids: Vec::new(),
                prev_designs: Vec::new(),
                cleared_active_alert: None,
                rule_changes: Vec::new(),
                created_plugin_id: None,
                prev_plugin: None,
            },
            displaced: existing.map(|o| o.displaced.clone()).unwrap_or_default(),
            designs: Vec::new(),
            rule_ids: Vec::new(),
            plugin_id: None,
        };
        if let Some(old) = existing {
            for did in &old.design_ids {
                if let Ok(d) = self.design(did).await {
                    applied.snap.prev_designs.push(d);
                }
            }
            if let Some(pid) = &old.plugin_id {
                let pinfo = self.plugins().await.into_iter().find(|p| &p.id == pid);
                let zip_b64 = snapshot_plugin_zip(&self.plugins_dir(), pid)?.unwrap_or_default();
                if let Some(p) = pinfo
                    && !zip_b64.is_empty()
                {
                    applied.snap.prev_plugin = Some(PluginSnapshot { id: p.id, zip_b64, on: p.on });
                }
            }
        }

        // 3. Apply it. A failure anywhere from here (a step, or writing the record) takes back
        // exactly what was done, so a retry starts from the state it found.
        let outcome = async {
            self.apply_item(item, &pkg_bytes, existing, action, &mut applied).await?;
            let rec = StoreInstallRecord {
                item_id: item.id.clone(),
                kind: item.kind.key().into(),
                format: item.package.format.key().into(),
                title: item.title.clone(),
                version: item.version.clone(),
                creator_id: item.creator_id.clone(),
                creator_name: creator.name.clone(),
                package_sha256: item.package.sha256.clone(),
                design_ids: applied.designs.iter().map(|d| d.id.clone()).collect(),
                rule_ids: applied.rule_ids.clone(),
                plugin_id: applied.plugin_id.clone(),
                installed_at: existing.map_or_else(|| now().0, |e| e.installed_at),
                updated_at: now().0,
                displaced: applied.displaced.clone(),
            };
            let undo_entry = StoreUndoEntry {
                token: ferrocast_store::new_id(),
                action: action.into(),
                item_id: item.id.clone(),
                item_title: item.title.clone(),
                snapshot: applied.snap.clone(),
                created_at: now().0,
            };
            // The record and the undo are one commit: no installed item without its undo.
            self.db(move |s| repo::store_catalog::commit_install(s, &rec, &undo_entry)).await
        }
        .await;
        let (saved, undo) = match outcome {
            Ok(x) => x,
            Err(e) => {
                if let Err(again) = self.restore_snapshot(&item.id, &applied.snap).await {
                    tracing::error!(error = %again, item = %item.id, "a failed Store install couldn't be fully taken back");
                    return Err(refused(
                        500,
                        "store_install_partial",
                        format!("{} couldn't be installed ({e}), and some of it couldn't be taken back.", item.title),
                        "Open Store, Installed to see what is there, then try Uninstall or Install again.",
                    ));
                }
                return Err(e);
            }
        };
        let installed = Some(installed_view(&saved, Some(std::slice::from_ref(item)), Some(&undo)));
        Ok(StoreMutationResult { action: action.into(), installed, designs: applied.designs, undo })
    }

    /// `POST /api/v1/store/install` `{ id }`
    pub async fn install_store_item(&self, id: &str) -> Result<StoreMutationResult, EngineError> {
        let _one_at_a_time = STORE_WRITE.lock().await;
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
        let _one_at_a_time = STORE_WRITE.lock().await;
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
    /// It either removes everything and says so, or leaves things as they were and says why: each
    /// step that fails takes back the steps before it, and the install record is removed last,
    /// together with the undo entry, so a retry always finds the item still installed.
    pub async fn uninstall_store_item(&self, id: &str) -> Result<StoreMutationResult, EngineError> {
        let _one_at_a_time = STORE_WRITE.lock().await;
        let existing = self
            .db({
                let id = id.to_string();
                move |s| repo::store_catalog::get_install(s, &id)
            })
            .await?
            .ok_or_else(|| {
                refused(404, "store_item_not_found", "That store item isn't installed.", "Reload Installed items.")
            })?;
        let mut snap = StoreUndoSnapshot {
            prev_install: Some(existing.clone()),
            created_design_ids: Vec::new(),
            prev_designs: Vec::new(),
            cleared_active_alert: None,
            rule_changes: Vec::new(),
            created_plugin_id: None,
            prev_plugin: None,
        };
        let done = self.remove_item_parts(&existing, &mut snap).await;
        let undo = match done {
            Ok(()) => {
                let entry = StoreUndoEntry {
                    token: ferrocast_store::new_id(),
                    action: "uninstall".into(),
                    item_id: existing.item_id.clone(),
                    item_title: existing.title.clone(),
                    snapshot: snap.clone(),
                    created_at: now().0,
                };
                let item_id = existing.item_id.clone();
                self.db(move |s| repo::store_catalog::commit_removal(s, &item_id, &entry)).await
            }
            Err(e) => Err(e),
        };
        match undo {
            Ok(undo) => {
                Ok(StoreMutationResult { action: "uninstall".into(), installed: None, designs: Vec::new(), undo })
            }
            Err(e) => {
                // Back to how it was, so "Uninstall" can simply be pressed again.
                if let Err(again) = self.restore_snapshot(&existing.item_id, &snap).await {
                    tracing::error!(error = %again, item = %existing.item_id, "a failed Store uninstall couldn't be fully taken back");
                    return Err(refused(
                        500,
                        "store_uninstall_partial",
                        format!("{} couldn't be removed ({e}), and some of it couldn't be put back.", existing.title),
                        "It is still listed under Installed. Try Uninstall again.",
                    ));
                }
                Err(refused(
                    500,
                    "store_uninstall_failed",
                    format!("{} couldn't be removed: {e}", existing.title),
                    "Nothing was changed. Try Uninstall again.",
                ))
            }
        }
    }

    /// Takes an installed item's parts away, writing each into `snap` as it goes.
    async fn remove_item_parts(
        &self,
        existing: &StoreInstallRecord,
        snap: &mut StoreUndoSnapshot,
    ) -> Result<(), EngineError> {
        let active = self.active_alert_design().await?;
        for did in &existing.design_ids {
            if let Ok(d) = self.design(did).await {
                snap.prev_designs.push(d);
            }
        }
        for did in &existing.design_ids {
            if active.as_deref() == Some(did.as_str()) && self.active_alert_design().await?.is_some() {
                self.set_active_alert_design(None).await?;
                snap.cleared_active_alert = Some(did.clone());
            }
            self.delete_design_row(did).await?;
        }
        let prefix = format!("store:{}:", existing.item_id);
        for rule_id in &existing.rule_ids {
            let Some(mut r) = self.rules().into_iter().find(|r| &r.id == rule_id) else { continue };
            let taken: Vec<Variation> = r.variations.iter().filter(|v| v.id.starts_with(&prefix)).cloned().collect();
            if taken.is_empty() {
                continue;
            }
            r.variations.retain(|v| !v.id.starts_with(&prefix));
            let mut change = RuleChange { rule_id: r.id.clone(), removed: taken, added: Vec::new(), created: false };
            if !r.variations.iter().any(|v| v.condition.is_none()) {
                // The look the pack took the place of goes back; with none kept, the built-in default.
                let back = existing
                    .displaced
                    .iter()
                    .find(|d| d.rule_id == r.id)
                    .map(|d| d.variation.clone())
                    .or_else(|| {
                        let kind = r.kinds.first().map(String::as_str).unwrap_or("follow");
                        default_rules()
                            .into_iter()
                            .find(|d| d.kinds.iter().any(|k| k == kind))
                            .and_then(|d| d.variations.into_iter().next())
                    })
                    .unwrap_or_else(|| Variation::new("default", "{name}"));
                if !r.variations.iter().any(|v| v.id == back.id) {
                    change.added.push(back.id.clone());
                    r.variations.insert(0, back);
                }
            }
            self.save_rule(r).await?;
            snap.rule_changes.push(change);
        }
        if let Some(pid) = &existing.plugin_id {
            let pinfo = self.plugins().await.into_iter().find(|p| &p.id == pid);
            if let Some(zip_b64) = snapshot_plugin_zip(&self.plugins_dir(), pid)?
                && let Some(p) = pinfo
            {
                snap.prev_plugin = Some(PluginSnapshot { id: p.id, zip_b64, on: p.on });
            }
            match self.remove_plugin(pid).await {
                Ok(()) | Err(EngineError::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// `POST /api/v1/store/undo` `{ token? }` — one-click undo of the latest install, update or
    /// uninstall. No token is "the latest"; a token that is empty, unknown or not the latest is
    /// refused (undoing an older one under a newer one would put back a state that no longer exists).
    pub async fn undo_store_action(&self, token: Option<&str>) -> Result<StoreUndoResult, EngineError> {
        let _one_at_a_time = STORE_WRITE.lock().await;
        if token.is_some_and(|t| t.trim().is_empty()) {
            return Err(refused(
                400,
                "store_undo_bad_token",
                "The undo token is empty.",
                "Leave the token out to undo the latest action, or send the one it gave.",
            ));
        }
        let latest = self.db(repo::store_catalog::latest_undo).await?.ok_or_else(|| {
            refused(
                404,
                "store_nothing_to_undo",
                "There is nothing to undo.",
                "Undo is kept for the last few Store actions.",
            )
        })?;
        if let Some(t) = token
            && t != latest.token
        {
            let tok = t.to_string();
            let known = self.db(move |s| repo::store_catalog::peek_undo(s, Some(&tok))).await.is_ok();
            return Err(if known {
                refused(
                    409,
                    "store_undo_out_of_order",
                    "That isn't the latest Store action, so it can't be undone first.",
                    "Undo the latest one, then this one.",
                )
            } else {
                refused(
                    404,
                    "store_undo_unknown",
                    "That undo isn't there any more.",
                    "Undo is kept for the last few Store actions.",
                )
            });
        }
        // The entry stays until everything is put back: a failed undo can be tried again.
        let tok = latest.token.clone();
        let entry = self.db(move |s| repo::store_catalog::peek_undo(s, Some(&tok))).await?;
        self.restore_snapshot(&entry.item_id, &entry.snapshot).await.map_err(|e| {
            refused(
                500,
                "store_undo_incomplete",
                format!("Undo couldn't put everything back: {e}"),
                "Nothing is lost: the undo is still there. Try it again.",
            )
        })?;
        let used = entry.token.clone();
        self.db(move |s| repo::store_catalog::drop_undo(s, &used)).await?;
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

    /// A signature made for the plugin catalog (same key, any key) can't open the Store, and the
    /// reverse: the signed file name is the domain.
    #[test]
    fn store_and_plugin_signatures_do_not_pass_as_each_other() {
        const PLUGIN_LIST: &[u8] = include_bytes!("../testdata/catalog/catalog.json");
        const PLUGIN_SIG: &str = include_str!("../testdata/catalog/catalog.json.sig");
        const PLUGIN_KEY: &str = include_str!("../testdata/catalog/test-key.pub");
        assert_eq!(verify_store(CATALOG_V1, CATALOG_V1_SIG, TEST_KEY).unwrap().file, STORE_CATALOG_FILE);
        // A real plugin-catalog signature isn't a Store signature...
        assert!(verify_store(PLUGIN_LIST, PLUGIN_SIG, PLUGIN_KEY).is_err(), "plugin signature opened the Store");
        // ...and a Store signature isn't a plugin-catalog one, even checked with the Store's key.
        assert!(crate::plugin_catalog::verify(CATALOG_V1, CATALOG_V1_SIG, TEST_KEY).is_err());
    }

    /// An older catalog than the last accepted one is a replay and is refused.
    #[tokio::test]
    async fn an_older_signed_catalog_is_refused_after_a_newer_one() {
        let _lock = TEST_GUARD.lock().await;
        let state = Arc::new(StdMutex::new(MockFixtureState::default()));
        let port = spawn_fixture_server(state.clone()).await;
        let (engine, _dir, _studio, _chat) = crate::automation::tests::engine().await;
        set_source(Source { url: format!("http://127.0.0.1:{port}/catalog.json"), key: TEST_KEY.into(), local: true });
        {
            let mut st = state.lock().unwrap();
            st.catalog = CATALOG_V2.to_vec();
            st.sig = CATALOG_V2_SIG.into();
        }
        engine.store_catalog(true).await.unwrap();
        {
            let mut st = state.lock().unwrap();
            st.catalog = CATALOG_V1.to_vec();
            st.sig = CATALOG_V1_SIG.into();
        }
        let old = engine.store_catalog(true).await;
        assert!(
            matches!(old, Err(EngineError::Refused { status: 502, code: "store_catalog_rollback", .. })),
            "{old:?}"
        );
        // The newer one still opens.
        {
            let mut st = state.lock().unwrap();
            st.catalog = CATALOG_V2.to_vec();
            st.sig = CATALOG_V2_SIG.into();
        }
        engine.store_catalog(true).await.unwrap();
    }

    /// Two refreshes that finish in the other order than they started: the older catalog must not
    /// end up in the cache (or the newer one's floor be passed by it).
    #[tokio::test]
    async fn overlapping_refreshes_publish_in_the_order_they_were_accepted() {
        let _lock = TEST_GUARD.lock().await;
        let (engine, _dir, _studio, _chat) = crate::automation::tests::engine().await;
        let src = Source { url: "http://127.0.0.1:1/catalog.json".into(), key: TEST_KEY.into(), local: true };
        set_source(src.clone());
        let older = parse_and_validate_catalog(CATALOG_V1, &src).unwrap();
        let newer = parse_and_validate_catalog(CATALOG_V2, &src).unwrap();
        let (older_ts, newer_ts) = (
            verify_store(CATALOG_V1, CATALOG_V1_SIG, TEST_KEY).unwrap().timestamp,
            verify_store(CATALOG_V2, CATALOG_V2_SIG, TEST_KEY).unwrap().timestamp,
        );
        assert!(newer.generated_at > older.generated_at && newer_ts >= older_ts);
        // The older one is accepted first but is held up inside; the newer one is accepted meanwhile.
        let hold = Arc::new(tokio::sync::Notify::new());
        *PUBLISH_PAUSE.lock().unwrap() = Some(hold.clone());
        let (e1, e2) = (engine.clone(), engine.clone());
        let a = tokio::spawn(async move { e1.publish_catalog(older_ts, older).await.map(|(c, _)| c.generated_at) });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let b = tokio::spawn(async move { e2.publish_catalog(newer_ts, newer).await.map(|(c, _)| c.generated_at) });
        tokio::time::sleep(Duration::from_millis(100)).await;
        hold.notify_one();
        let (a, b) = (a.await.unwrap(), b.await.unwrap());
        assert!(a.is_ok() && b.is_ok(), "{a:?} {b:?}");
        let cached = STORE_CACHE.lock().unwrap().clone().expect("a catalog is cached");
        assert_eq!(cached.catalog.generated_at, b.unwrap(), "the newer catalog is the one that stays");
        // And the older one can't come back afterwards.
        let again = parse_and_validate_catalog(CATALOG_V1, &src).unwrap();
        let late = engine.publish_catalog(older_ts, again).await;
        assert!(matches!(late, Err(EngineError::Refused { code: "store_catalog_rollback", .. })), "{late:?}");
    }

    /// A Store plugin whose id is already installed by the user is refused and left exactly as it
    /// was: the Store never overwrites it, and an Undo never removes it.
    #[tokio::test]
    async fn a_store_plugin_never_overwrites_a_plugin_the_user_already_has() {
        let _lock = TEST_GUARD.lock().await;
        let state = Arc::new(StdMutex::new(MockFixtureState::default()));
        let port = spawn_fixture_server(state.clone()).await;
        let (engine, dir, _studio, _chat) = crate::automation::tests::engine().await;
        set_source(Source { url: format!("http://127.0.0.1:{port}/catalog.json"), key: TEST_KEY.into(), local: true });
        // The user's own install of the same plugin id (the package is the catalog's, but it came
        // from outside the Store).
        let zip = dir.path().join("mine.zip");
        std::fs::write(&zip, PKG_PLUGIN).unwrap();
        engine.install_plugin(&zip).await.unwrap();
        let before = snapshot_plugin_zip(&engine.plugins_dir(), "dev.kira.softvignette").unwrap();
        assert!(before.is_some());
        let r = engine.install_store_item("soft-dimmer-shader").await;
        assert!(matches!(r, Err(EngineError::Refused { status: 409, code: "store_plugin_exists", .. })), "{r:?}");
        let after = snapshot_plugin_zip(&engine.plugins_dir(), "dev.kira.softvignette").unwrap();
        assert_eq!(before, after, "the user's plugin changed");
        assert!(engine.store_installed().await.unwrap().installed.is_empty());
    }

    /// The package's own id must be the one the catalog lists for the item.
    #[test]
    fn a_plugin_package_must_carry_the_id_the_catalog_lists() {
        assert!(plugin_identity_problem("dev.kira.softvignette", "dev.kira.softvignette").is_none());
        assert!(plugin_identity_problem("dev.kira.softvignette", "dev.evil.other").is_some());
        assert!(plugin_identity_problem("dev.kira.softvignette", "../escape").is_some());
    }

    /// Creator links never point at this machine or the local network, in any address spelling.
    #[test]
    fn creator_links_refuse_every_private_or_loopback_spelling() {
        for bad in [
            "https://[::1]/",
            "https://[::ffff:127.0.0.1]/",
            "https://[::ffff:10.0.0.5]/",
            "https://[fc00::1]/",
            "https://[fd12:3456::1]/",
            "https://[fe80::1]/",
            "https://[::]/",
            "https://0.0.0.0/",
            "https://127.0.0.2/",
            "https://169.254.169.254/",
            "https://10.1.2.3/",
            "https://192.168.1.1/",
            "https://172.16.0.1/",
            "https://100.64.0.1/",
            "https://localhost/",
            "https://printer.local/",
            "https://LOCALHOST./",
        ] {
            assert!(check_creator_link("link", bad).is_err(), "{bad} was accepted");
        }
        for good in ["https://example.com/me", "https://github.com/user", "https://[2606:4700::1111]/"] {
            assert!(check_creator_link("link", good).is_ok(), "{good} was refused");
        }
    }

    #[test]
    fn an_asset_must_start_like_the_type_it_declares() {
        assert!(asset_matches_mime("image/png", ASSET_AURORA_PNG));
        assert!(asset_matches_mime("image/png", ASSET_ARCADE_PNG));
        assert!(!asset_matches_mime("image/jpeg", ASSET_AURORA_PNG), "png bytes declared as jpeg");
        assert!(!asset_matches_mime("image/png", b"<svg onload=alert(1)></svg>"), "markup declared as png");
        assert!(!asset_matches_mime("image/png", b"<!doctype html><script>1</script>"));
        assert!(!asset_matches_mime("text/html", b"<html>"), "a type nobody serves");
        assert!(asset_matches_mime("image/gif", b"GIF89a\x01\x00"));
        assert!(asset_matches_mime("audio/ogg", b"OggS\x00"));
    }

    /// What the catalog may call a version is exactly what the update comparison can read: a
    /// version that passed admission but can't be compared would hide every later update.
    #[test]
    fn a_store_version_is_one_the_update_comparison_can_read() {
        assert!(check_version("version", "2.1.0").is_ok() && check_version("version", "1.0.0-beta.2").is_ok());
        for bad in ["2oops", "1.0.0-", "1..2", "9a", "1.0.0-a b"] {
            assert!(check_version("version", bad).is_err(), "{bad}");
            assert!(check_version("uri.v", bad).is_err(), "{bad}");
        }
        assert!(check_version("version", &"1".repeat(33)).is_err(), "still at most 32 characters");
    }

    /// A link's confirmation doesn't claim checks that haven't run: nothing is downloaded yet.
    #[test]
    fn a_preview_says_what_will_be_checked_not_that_it_was() {
        for f in [StorePackageFormat::Ferrodesign, StorePackageFormat::Ferroalert, StorePackageFormat::Ferroplugin] {
            let notes = preview_notes(f, 4096).join(" | ");
            assert!(notes.contains("signature was checked"), "{notes}");
            assert!(notes.contains("when you install"), "{f:?}: {notes}");
            for claimed in ["verified by", "Sandboxed plugin verified", "verified (", "p2e"] {
                assert!(!notes.contains(claimed), "{f:?} claims a check that hasn't run ({claimed}): {notes}");
            }
        }
        assert!(preview_notes(StorePackageFormat::Ferrodesign, 10)[1].contains("1 KB"));
    }

    #[test]
    fn a_ferrocast_link_has_no_port_or_fragment_and_only_a_version_parameter() {
        assert!(parse_ferrocast_link("ferrocast://store/install/aurora-pulse").is_ok());
        assert!(parse_ferrocast_link("ferrocast://store/install/aurora-pulse?v=1.0.0").is_ok());
        for bad in [
            "ferrocast://store:8080/install/aurora-pulse",
            "ferrocast://store/install/aurora-pulse#frag",
            "ferrocast://store/install/aurora-pulse?v=1.0.0&x=1",
            "ferrocast://store/install/aurora-pulse?x=1",
            "ferrocast://evil/install/aurora-pulse",
            "ferrocast://store/install/../aurora-pulse",
            "ferrocast://store/install/aurora-pulse/extra",
            "ferrocast://user@store/install/aurora-pulse",
        ] {
            assert!(parse_ferrocast_link(bad).is_err(), "{bad} was accepted");
        }
    }

    /// Two installs of one item at once: exactly one goes through (the second finds it installed),
    /// and nothing is imported twice.
    #[tokio::test]
    async fn two_installs_at_once_install_once() {
        let _lock = TEST_GUARD.lock().await;
        let state = Arc::new(StdMutex::new(MockFixtureState::default()));
        let port = spawn_fixture_server(state.clone()).await;
        let (engine, _dir, _studio, _chat) = crate::automation::tests::engine().await;
        set_source(Source { url: format!("http://127.0.0.1:{port}/catalog.json"), key: TEST_KEY.into(), local: true });
        let (a, b) = tokio::join!(engine.install_store_item("aurora-pulse"), engine.install_store_item("aurora-pulse"));
        assert_eq!([a.is_ok(), b.is_ok()].iter().filter(|x| **x).count(), 1, "{a:?} {b:?}");
        let designs = engine.designs(None).await.unwrap();
        assert_eq!(designs.len(), engine.store_installed().await.unwrap().installed[0].design_ids.len());
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
        // A preview asset isn't installed (and so isn't downloaded at install): a changed one blocks
        // showing it, not installing the package.
        let installed = engine.install_store_item("aurora-pulse").await;
        assert!(installed.is_ok(), "{installed:?}");
        let installed = installed.unwrap();
        engine.undo_store_action(Some(&installed.undo.token)).await.unwrap();
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

    // ---- Store actions restore exactly the prior state and touch only what they changed ----

    async fn fixture() -> (Engine, Arc<StdMutex<MockFixtureState>>, tempfile::TempDir) {
        let state = Arc::new(StdMutex::new(MockFixtureState::default()));
        let port = spawn_fixture_server(state.clone()).await;
        let (engine, dir, _studio, _chat) = crate::automation::tests::engine().await;
        set_source(Source { url: format!("http://127.0.0.1:{port}/catalog.json"), key: TEST_KEY.into(), local: true });
        (engine, state, dir)
    }

    fn follow_rule(engine: &Engine) -> ferrocast_core::alert::AlertRule {
        engine.rules().into_iter().find(|r| r.kinds.contains(&"follow".to_string())).expect("a follow rule")
    }

    /// The person's own follow look, saved before the pack comes.
    async fn my_own_follow_look(engine: &Engine) -> Variation {
        let mut rule = default_rules().into_iter().find(|r| r.kinds.contains(&"follow".to_string())).unwrap();
        rule.variations[0].title = "{name} joined MY crew!".into();
        rule.variations[0].volume = 33;
        let mine = rule.variations[0].clone();
        engine.save_rule(rule).await.unwrap();
        mine
    }

    /// Breaks or mends a table so a step of a Store action fails, whatever the step is.
    async fn table(engine: &Engine, name: &'static str, broken: bool) {
        let sql = if broken {
            format!("ALTER TABLE {name} RENAME TO {name}_away")
        } else {
            format!("ALTER TABLE {name}_away RENAME TO {name}")
        };
        engine.db(move |s| s.write(|tx| Ok(tx.execute_batch(&sql)?))).await.unwrap();
    }

    #[tokio::test]
    async fn removing_an_alert_pack_gives_back_the_look_it_displaced() {
        let _lock = TEST_GUARD.lock().await;
        let (engine, _state, _dir) = fixture().await;
        let mine = my_own_follow_look(&engine).await;
        let installed = engine.install_store_item("arcade-ticker-alert").await.unwrap();
        let rule = follow_rule(&engine);
        assert!(rule.variations.iter().any(|v| v.id.starts_with("store:arcade-ticker-alert:")));
        assert!(!rule.variations.iter().any(|v| v.id == mine.id), "the pack took the default place");
        // The look is kept with the install, not only in the undo (which the next action replaces).
        assert_eq!(installed.installed.as_ref().unwrap().rule_ids.len(), 1);
        engine.install_store_item("foundry-goal-widget").await.unwrap();
        engine.uninstall_store_item("arcade-ticker-alert").await.unwrap();
        let back = follow_rule(&engine);
        assert!(!back.variations.iter().any(|v| v.id.starts_with("store:")), "nothing of the pack is left");
        let default: Vec<_> = back.variations.iter().filter(|v| v.condition.is_none()).collect();
        assert_eq!(default.len(), 1, "one default");
        assert_eq!(default[0], &mine, "the person's own title, sound and volume, not a built-in one");
    }

    #[tokio::test]
    async fn undo_reverses_only_what_the_store_changed_so_later_edits_stay() {
        let _lock = TEST_GUARD.lock().await;
        let (engine, _state, _dir) = fixture().await;
        let mine = my_own_follow_look(&engine).await;
        let installed = engine.install_store_item("arcade-ticker-alert").await.unwrap();
        // After the install the person adds a look of their own and edits another setting.
        let mut rule = follow_rule(&engine);
        let mut extra = Variation::new("my-big-follow", "BIG {name}");
        extra.condition = Some(ferrocast_core::alert::Condition::TierAtLeast { tier: 2 });
        rule.variations.push(extra.clone());
        rule.name = "My follows".into();
        engine.save_rule(rule).await.unwrap();
        engine.undo_store_action(Some(&installed.undo.token)).await.unwrap();
        let after = follow_rule(&engine);
        assert_eq!(after.name, "My follows", "an edit made after the install is not overwritten");
        assert!(after.variations.iter().any(|v| v.id == "my-big-follow"), "the look added since stays");
        assert!(!after.variations.iter().any(|v| v.id.starts_with("store:")), "the pack's variations are gone");
        assert_eq!(after.variations.iter().find(|v| v.condition.is_none()), Some(&mine), "the default is back");
    }

    #[tokio::test]
    async fn a_failed_uninstall_is_not_a_success_and_can_be_pressed_again() {
        let _lock = TEST_GUARD.lock().await;
        let (engine, _state, _dir) = fixture().await;
        let mine = my_own_follow_look(&engine).await;
        engine.install_store_item("arcade-ticker-alert").await.unwrap();
        // The rules can't be saved: the cleanup of the pack's variations fails.
        table(&engine, "alert_rules", true).await;
        let failed = engine.uninstall_store_item("arcade-ticker-alert").await;
        table(&engine, "alert_rules", false).await;
        assert!(failed.is_err(), "{failed:?}");
        // Still installed, with its undo untouched, and the rule as it was.
        let still = engine.store_installed().await.unwrap();
        assert!(
            still.installed.iter().any(|i| i.item_id == "arcade-ticker-alert"),
            "the record stays: nothing is lost"
        );
        assert!(follow_rule(&engine).variations.iter().any(|v| v.id.starts_with("store:arcade-ticker-alert:")));
        // Mended, the same press finishes the job.
        engine.uninstall_store_item("arcade-ticker-alert").await.unwrap();
        assert!(engine.store_installed().await.unwrap().installed.is_empty());
        assert_eq!(follow_rule(&engine).variations.iter().find(|v| v.condition.is_none()), Some(&mine));
    }

    #[tokio::test]
    async fn an_undo_that_cannot_finish_stays_available_and_finishes_on_retry() {
        let _lock = TEST_GUARD.lock().await;
        let (engine, _state, _dir) = fixture().await;
        engine.install_store_item("aurora-pulse").await.unwrap();
        let un = engine.uninstall_store_item("aurora-pulse").await.unwrap();
        assert!(engine.designs(None).await.unwrap().is_empty());
        // The designs can't be written back.
        table(&engine, "designs", true).await;
        let failed = engine.undo_store_action(Some(&un.undo.token)).await;
        table(&engine, "designs", false).await;
        assert!(matches!(failed, Err(EngineError::Refused { code: "store_undo_incomplete", .. })), "{failed:?}");
        // Not reported as done, and the same token is still good.
        assert!(engine.designs(None).await.unwrap().is_empty());
        let done = engine.undo_store_action(Some(&un.undo.token)).await.unwrap();
        assert_eq!(done.reverted_action, "uninstall");
        assert_eq!(engine.designs(None).await.unwrap().len(), 2, "both designs are back");
        // The next press is the one before it (the install), as a stack.
        let before = engine.undo_store_action(None).await.unwrap();
        assert_eq!(before.reverted_action, "install");
        assert!(engine.designs(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_install_that_fails_after_the_package_was_applied_is_taken_back() {
        let _lock = TEST_GUARD.lock().await;
        let (engine, _state, _dir) = fixture().await;
        // The record and its undo are written together: with no place for undos, that fails.
        table(&engine, "store_undo", true).await;
        let failed = engine.install_store_item("aurora-pulse").await;
        table(&engine, "store_undo", false).await;
        assert!(failed.is_err(), "{failed:?}");
        assert!(engine.designs(None).await.unwrap().is_empty(), "the designs it made were taken back");
        assert!(engine.store_installed().await.unwrap().installed.is_empty(), "no record of a half install");
        // So a retry is a plain install, not "already installed", and has its undo.
        let again = engine.install_store_item("aurora-pulse").await.unwrap();
        assert_eq!(engine.designs(None).await.unwrap().len(), 2);
        assert!(again.installed.unwrap().can_undo);
        // The same for an alert pack: its variations don't stay in the rule.
        let mine = my_own_follow_look(&engine).await;
        table(&engine, "store_undo", true).await;
        let failed = engine.install_store_item("arcade-ticker-alert").await;
        table(&engine, "store_undo", false).await;
        assert!(failed.is_err());
        let rule = follow_rule(&engine);
        assert!(!rule.variations.iter().any(|v| v.id.starts_with("store:")));
        assert_eq!(rule.variations.iter().find(|v| v.condition.is_none()), Some(&mine));
    }

    #[tokio::test]
    async fn undo_tokens_are_the_latest_or_refused_never_the_latest_by_default() {
        let _lock = TEST_GUARD.lock().await;
        let (engine, _state, _dir) = fixture().await;
        assert!(
            matches!(engine.undo_store_action(None).await, Err(EngineError::Refused { status: 404, .. })),
            "nothing yet"
        );
        let first = engine.install_store_item("aurora-pulse").await.unwrap();
        let second = engine.install_store_item("foundry-goal-widget").await.unwrap();
        // Empty or blank: refused, not "undo the latest".
        for bad in ["", "   "] {
            let r = engine.undo_store_action(Some(bad)).await;
            assert!(
                matches!(r, Err(EngineError::Refused { status: 400, code: "store_undo_bad_token", .. })),
                "{bad:?}: {r:?}"
            );
        }
        // Unknown: refused.
        let r = engine.undo_store_action(Some("nope")).await;
        assert!(matches!(r, Err(EngineError::Refused { status: 404, code: "store_undo_unknown", .. })), "{r:?}");
        // Older than the latest: refused, and nothing changed.
        let r = engine.undo_store_action(Some(&first.undo.token)).await;
        assert!(matches!(r, Err(EngineError::Refused { status: 409, code: "store_undo_out_of_order", .. })), "{r:?}");
        assert_eq!(engine.store_installed().await.unwrap().installed.len(), 2);
        // The latest by its token, then the one before.
        engine.undo_store_action(Some(&second.undo.token)).await.unwrap();
        assert_eq!(engine.store_installed().await.unwrap().installed.len(), 1);
        // Leaving the token out is the way to ask for the latest.
        let r = engine.undo_store_action(None).await.unwrap();
        assert_eq!(r.item_id, "aurora-pulse");
        assert!(engine.designs(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_alert_pack_with_no_rule_to_go_into_is_refused_not_recorded() {
        let _lock = TEST_GUARD.lock().await;
        let (engine, _state, _dir) = fixture().await;
        // Only a tip rule: the pack is for follows.
        for r in engine.rules() {
            engine.delete_rule(&r.id).await.unwrap();
        }
        let tip = default_rules().into_iter().find(|r| r.kinds.contains(&"tip".to_string())).unwrap();
        engine.save_rule(tip).await.unwrap();
        let r = engine.install_store_item("arcade-ticker-alert").await;
        assert!(matches!(r, Err(EngineError::Refused { status: 422, code: "store_alert_no_target", .. })), "{r:?}");
        assert!(engine.store_installed().await.unwrap().installed.is_empty(), "nothing is shown as installed");
        assert!(engine.rules().iter().all(|r| !r.variations.iter().any(|v| v.id.starts_with("store:"))));
        assert!(engine.db(repo::store_catalog::latest_undo).await.unwrap().is_none(), "and no undo for nothing");
    }

    #[test]
    fn an_update_finds_the_packs_own_default_in_the_slot_not_the_users() {
        let mine = Variation::new("default", "mine");
        let theirs = {
            let mut v = Variation::new("pack-default", "pack");
            v.condition = None;
            v
        };
        let first = merge_pack(std::slice::from_ref(&mine), "p", std::slice::from_ref(&theirs));
        assert_eq!(first.displaced.as_ref(), Some(&mine));
        assert_eq!(first.added, vec!["store:p:pack-default".to_string()]);
        // Again (an update): the default there is the pack's own, so nothing of the user's is "displaced".
        let again = merge_pack(&first.variations, "p", std::slice::from_ref(&theirs));
        assert!(again.displaced.is_none(), "the user's look stays recorded from the first install");
        assert_eq!(again.removed.len(), 1, "the pack's own earlier variation is what comes out");
        assert_eq!(again.variations.iter().filter(|v| v.condition.is_none()).count(), 1);
    }
}
