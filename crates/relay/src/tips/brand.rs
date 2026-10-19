//! Pro tip page branding (docs/plan/PRO.md F6): the streamer's own colours, avatar and
//! banner on their public tip page.
//!
//! Colours are three hex values. Text that would be hard to read on the background (under
//! 4.5:1, WCAG AA) is moved towards white or black until it passes, and the accent until it
//! stands out (3:1 for buttons and bars); the answer says what changed. Images arrive as raw
//! PNG, JPEG or WebP, are decoded, shrunk (avatar 512 × 512, banner 1500 × 500 at most),
//! turned upright and re-encoded as lossless WebP from the pixels alone, so EXIF (GPS,
//! camera), ICC and XMP never reach the page. The result is stored by its SHA-256
//! (`t/a/<sha>.webp` in the `BlobStore`) and served immutable at `/t/a/<sha>.webp` on the
//! relay and the tip host.
//!
//! The page uses the branding only while the install is Pro; a lapsed install shows the
//! default look, and its branding is deleted 30 days after the hourly sweep first saw it
//! lapsed (R-04).

use std::io::Cursor;

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use rsa::sha2::{Digest, Sha256};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

use super::Body as JsonBody;
use crate::billing::gate::require_pro;
use crate::blob::{drain, drain_stream, human_bytes};
use crate::db::Db;
use crate::quota::{Limit, Period};
use crate::{ApiError, AppState, now_ms};

/// Migration 3's tables (with `goal::SCHEMA`).
pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tip_brand (install_id TEXT PRIMARY KEY, accent TEXT, background TEXT, text TEXT, avatar TEXT, banner TEXT, updated_at INTEGER NOT NULL, lapsed_at INTEGER);
";

/// Image uploads per subscription per UTC day (decoding is the expensive part).
pub(crate) const IMAGES_PER_DAY: Limit = Limit { name: "tip_images", period: Period::Day, limit: 30 };

/// Largest image a decode may produce, in pixels (4096 × 4096). Bigger ones are refused
/// before decoding: the relay's container is small.
const MAX_PIXELS: u64 = 4096 * 4096;
const MAX_SIDE: u32 = 8192;
/// Text needs 4.5:1 on the background (WCAG AA), the accent 3:1 (buttons, bars).
pub const TEXT_CONTRAST: f64 = 4.5;
pub const ACCENT_CONTRAST: f64 = 3.0;

/// Which image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Avatar,
    Banner,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "avatar" => Some(Kind::Avatar),
            "banner" => Some(Kind::Banner),
            _ => None,
        }
    }
    /// The largest upload.
    pub fn max_bytes(self) -> u64 {
        match self {
            Kind::Avatar => 1_000_000,
            Kind::Banner => 3_000_000,
        }
    }
    /// What the stored image fits in.
    pub fn max_size(self) -> (u32, u32) {
        match self {
            Kind::Avatar => (512, 512),
            Kind::Banner => (1500, 500),
        }
    }
    fn column(self) -> &'static str {
        match self {
            Kind::Avatar => "avatar",
            Kind::Banner => "banner",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Kind::Avatar => "Avatars",
            Kind::Banner => "Banners",
        }
    }
}

// ── Colours ──────────────────────────────────────────────────────────────────────────

/// An sRGB colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// `#rrggbb` or `#rgb`, any case.
    pub fn parse(s: &str) -> Option<Rgb> {
        let h = s.trim().strip_prefix('#')?;
        if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok();
        let nib = |i: usize| u8::from_str_radix(h.get(i..=i)?, 16).ok().map(|n| n * 17);
        match h.len() {
            6 => Some(Rgb(byte(0)?, byte(2)?, byte(4)?)),
            3 => Some(Rgb(nib(0)?, nib(1)?, nib(2)?)),
            _ => None,
        }
    }

    pub fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }

    /// WCAG relative luminance.
    pub fn luminance(self) -> f64 {
        let ch = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.039_28 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * ch(self.0) + 0.7152 * ch(self.1) + 0.0722 * ch(self.2)
    }

    /// `self` moved `t` (0–1) of the way to `other`.
    pub fn mix(self, other: Rgb, t: f64) -> Rgb {
        let m = |a: u8, b: u8| {
            let v = f64::from(a) + (f64::from(b) - f64::from(a)) * t.clamp(0.0, 1.0);
            // In 0–255 after the clamp, so the cast can't truncate.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let out = v.round().clamp(0.0, 255.0) as u8;
            out
        };
        Rgb(m(self.0, other.0), m(self.1, other.1), m(self.2, other.2))
    }
}

/// The WCAG contrast ratio of two colours (1–21).
pub fn contrast(a: Rgb, b: Rgb) -> f64 {
    let (x, y) = (a.luminance(), b.luminance());
    let (hi, lo) = if x > y { (x, y) } else { (y, x) };
    (hi + 0.05) / (lo + 0.05)
}

const WHITE: Rgb = Rgb(255, 255, 255);

/// Orange or amber: a hue of about 15-45 degrees with clear saturation.
fn is_orange(c: Rgb) -> bool {
    let (r, g, b) = (f64::from(c.0), f64::from(c.1), f64::from(c.2));
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    if max - min < 60.0 || max != r || g < b {
        return false;
    }
    let hue = 60.0 * (g - b) / (max - min);
    (15.0..=45.0).contains(&hue)
}

/// A filled button's background and label. On orange the label is always white, with the
/// fill darkened (same hue) until white meets WCAG AA (4.5:1); otherwise the better of
/// white and black on the accent itself.
fn button_colours(accent: Rgb) -> (Rgb, Rgb) {
    if is_orange(accent) {
        let mut fill = accent;
        let mut t = 0.0;
        while contrast(WHITE, fill) < TEXT_CONTRAST && t < 1.0 {
            t += 0.02;
            fill = accent.mix(BLACK, t);
        }
        return (fill, WHITE);
    }
    let on = if contrast(WHITE, accent) >= contrast(BLACK, accent) { WHITE } else { BLACK };
    (accent, on)
}
const BLACK: Rgb = Rgb(0, 0, 0);

/// `fg` as close to what was picked as possible with at least `min` contrast on `bg`:
/// moved towards white or black (whichever side reads better) in small steps.
pub fn readable(fg: Rgb, bg: Rgb, min: f64) -> Rgb {
    if contrast(fg, bg) >= min {
        return fg;
    }
    let target = if contrast(WHITE, bg) >= contrast(BLACK, bg) { WHITE } else { BLACK };
    for step in 1..=100 {
        let c = fg.mix(target, f64::from(step) / 100.0);
        if contrast(c, bg) >= min {
            return c;
        }
    }
    target
}

/// One automatic change to what the streamer picked.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub field: &'static str,
    pub from: Rgb,
    pub to: Rgb,
    pub before: f64,
    pub after: f64,
}

impl Change {
    fn json(&self) -> Value {
        let what = if self.field == "text" { "Text" } else { "The accent" };
        let need = if self.field == "text" { TEXT_CONTRAST } else { ACCENT_CONTRAST };
        json!({
            "field": self.field, "from": self.from.hex(), "to": self.to.hex(),
            "contrast_before": round1(self.before), "contrast_after": round1(self.after),
            "message": format!(
                "{what} was {:.1}:1 on your background, under {need}:1, so it's now {} ({:.1}:1) to stay readable.",
                self.before, self.to.hex(), self.after
            ),
        })
    }
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// The three colours, made readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colours {
    pub accent: Rgb,
    pub background: Rgb,
    pub text: Rgb,
}

impl Colours {
    /// Fixes text (4.5:1) and accent (3:1) against the background; the background is the
    /// streamer's choice and never changes.
    pub fn fixed(self) -> (Colours, Vec<Change>) {
        let mut out = self;
        let mut changes = Vec::new();
        for (field, slot, min) in [("text", &mut out.text, TEXT_CONTRAST), ("accent", &mut out.accent, ACCENT_CONTRAST)]
        {
            let from = *slot;
            let to = readable(from, self.background, min);
            if to != from {
                changes.push(Change {
                    field,
                    from,
                    to,
                    before: contrast(from, self.background),
                    after: contrast(to, self.background),
                });
                *slot = to;
            }
        }
        (out, changes)
    }
}

/// What the public page is drawn with: the default look, or the streamer's colours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    pub scheme: &'static str,
    pub bg: String,
    pub glow: String,
    pub fg: String,
    pub muted: String,
    pub accent: String,
    /// A filled button's colour: the accent, or for an orange accent a darker shade of it
    /// that white text reads on (Ferrocast's rule: text on orange is always white).
    pub accent_fill: String,
    /// Label on `accent_fill`.
    pub on_accent: String,
    pub surface: String,
    pub line: String,
}

impl Default for Palette {
    /// The free tip page's look.
    fn default() -> Palette {
        Palette {
            scheme: "dark",
            bg: "#0c0e12".into(),
            glow: "#1b2a5c".into(),
            fg: "#eef0f4".into(),
            muted: "#9aa3b2".into(),
            accent: "#3461f5".into(),
            accent_fill: "#3461f5".into(),
            on_accent: "#ffffff".into(),
            surface: "#151a24".into(),
            line: "#263047".into(),
        }
    }
}

impl Palette {
    pub fn from_colours(c: Colours) -> Palette {
        let (c, _) = c.fixed();
        let bg = c.background;
        let muted = {
            let m = c.text.mix(bg, 0.3);
            if contrast(m, bg) >= TEXT_CONTRAST { m } else { c.text }
        };
        let (accent_fill, on_accent) = button_colours(c.accent);
        Palette {
            scheme: if bg.luminance() < 0.4 { "dark" } else { "light" },
            bg: bg.hex(),
            glow: bg.mix(c.accent, 0.22).hex(),
            fg: c.text.hex(),
            muted: muted.hex(),
            accent: c.accent.hex(),
            accent_fill: accent_fill.hex(),
            on_accent: on_accent.hex(),
            surface: bg.mix(c.text, 0.06).hex(),
            line: bg.mix(c.text, 0.16).hex(),
        }
    }

    /// The CSS custom properties for `:root`.
    pub fn css_vars(&self) -> String {
        format!(
            "color-scheme:{};--bg:{};--glow:{};--fg:{};--muted:{};--accent:{};--accent-fill:{};--on-accent:{};--surface:{};--line:{}",
            self.scheme,
            self.bg,
            self.glow,
            self.fg,
            self.muted,
            self.accent,
            self.accent_fill,
            self.on_accent,
            self.surface,
            self.line
        )
    }
}

// ── Stored branding ──────────────────────────────────────────────────────────────────

/// One install's branding as stored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Brand {
    pub colours: Option<Colours>,
    /// SHA-256 (hex) of the stored WebP.
    pub avatar: Option<String>,
    pub banner: Option<String>,
}

impl Brand {
    pub fn palette(&self) -> Palette {
        self.colours.map(Palette::from_colours).unwrap_or_default()
    }
}

/// The public path of a stored image (same on the relay and the tip host).
pub fn asset_path(sha: &str) -> String {
    format!("/t/a/{sha}.webp")
}

fn blob_key(sha: &str) -> String {
    format!("t/a/{sha}.webp")
}

impl Db {
    pub fn tip_brand(&self, install: &str) -> rusqlite::Result<Option<Brand>> {
        self.c()
            .query_row(
                "SELECT accent, background, text, avatar, banner FROM tip_brand WHERE install_id = ?1",
                [install],
                |r| {
                    let col = |i: usize| -> rusqlite::Result<Option<Rgb>> {
                        Ok(r.get::<_, Option<String>>(i)?.as_deref().and_then(Rgb::parse))
                    };
                    let colours = match (col(0)?, col(1)?, col(2)?) {
                        (Some(accent), Some(background), Some(text)) => Some(Colours { accent, background, text }),
                        _ => None,
                    };
                    Ok(Brand { colours, avatar: r.get(3)?, banner: r.get(4)? })
                },
            )
            .optional()
    }

    /// Saves (or with `None` clears) the colours, keeping the images.
    pub fn set_tip_colours(&self, install: &str, c: Option<Colours>) -> rusqlite::Result<()> {
        let hex = |f: fn(&Colours) -> Rgb| c.as_ref().map(|c| f(c).hex());
        self.c().execute(
            "INSERT INTO tip_brand(install_id, accent, background, text, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(install_id) DO UPDATE SET accent = excluded.accent, background = excluded.background,
               text = excluded.text, updated_at = excluded.updated_at",
            params![install, hex(|c| c.accent), hex(|c| c.background), hex(|c| c.text), now_ms()],
        )?;
        Ok(())
    }

    /// Points an image at a stored blob (or clears it); returns the one it replaced.
    pub fn set_tip_image(&self, install: &str, kind: Kind, sha: Option<&str>) -> rusqlite::Result<Option<String>> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let col = kind.column();
        let old: Option<String> = tx
            .query_row(&format!("SELECT {col} FROM tip_brand WHERE install_id = ?1"), [install], |r| r.get(0))
            .optional()?
            .flatten();
        tx.execute(
            &format!(
                "INSERT INTO tip_brand(install_id, {col}, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(install_id) DO UPDATE SET {col} = excluded.{col}, updated_at = excluded.updated_at"
            ),
            params![install, sha, now_ms()],
        )?;
        tx.commit()?;
        Ok(old.filter(|o| Some(o.as_str()) != sha))
    }

    /// Whether any branding still uses this image (installs can share one by its hash).
    pub fn tip_image_used(&self, sha: &str) -> rusqlite::Result<bool> {
        self.c()
            .query_row("SELECT EXISTS(SELECT 1 FROM tip_brand WHERE avatar = ?1 OR banner = ?1)", [sha], |r| r.get(0))
    }
}

/// Removes images nothing uses any more. Failures only leave an orphan behind.
pub(crate) async fn forget_images(st: &AppState, shas: Vec<String>) {
    let Ok(blobs) = st.blobs() else { return };
    for sha in shas {
        match st.db.tip_image_used(&sha) {
            Ok(false) => {
                if let Err(e) = blobs.delete(&blob_key(&sha)).await {
                    tracing::warn!(error = %e, "couldn't delete a tip page image");
                }
            }
            Ok(true) => {}
            Err(e) => tracing::warn!(error = %e, "couldn't check a tip page image"),
        }
    }
}

// ── Images ───────────────────────────────────────────────────────────────────────────

/// Why an image was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageErr {
    /// Not PNG, JPEG or WebP (by its bytes, whatever the header said).
    Unsupported,
    /// Decodes to more than [`MAX_PIXELS`].
    TooManyPixels,
    /// Broken or not really an image.
    Unreadable,
}

/// Decodes, shrinks to fit `kind`, turns upright, and re-encodes as lossless WebP from the
/// pixels only (no metadata). CPU-heavy: run it off the async runtime.
pub fn process(kind: Kind, bytes: &[u8]) -> Result<Vec<u8>, ImageErr> {
    use image::{ImageDecoder, ImageFormat, metadata::Orientation};
    let format = image::guess_format(bytes).map_err(|_| ImageErr::Unsupported)?;
    if !matches!(format, ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP) {
        return Err(ImageErr::Unsupported);
    }
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(MAX_PIXELS * 4 + (8 << 20));
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(read_err)?;
    let (w, h) = decoder.dimensions();
    if w == 0 || h == 0 {
        return Err(ImageErr::Unreadable);
    }
    if u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(ImageErr::TooManyPixels);
    }
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let img = image::DynamicImage::from_decoder(decoder).map_err(read_err)?;
    // Shrink first (cheap on memory), then turn: a sideways photo fits the swapped box.
    let (mw, mh) = kind.max_size();
    let sideways = matches!(
        orientation,
        Orientation::Rotate90 | Orientation::Rotate270 | Orientation::Rotate90FlipH | Orientation::Rotate270FlipH
    );
    let (bw, bh) = if sideways { (mh, mw) } else { (mw, mh) };
    let mut img = if img.width() > bw || img.height() > bh { img.thumbnail(bw, bh) } else { img };
    img.apply_orientation(orientation);
    let mut out = Vec::new();
    let enc = image::codecs::webp::WebPEncoder::new_lossless(&mut out);
    let res = if img.color().has_alpha() {
        let px = img.to_rgba8();
        enc.encode(px.as_raw(), px.width(), px.height(), image::ExtendedColorType::Rgba8)
    } else {
        let px = img.to_rgb8();
        enc.encode(px.as_raw(), px.width(), px.height(), image::ExtendedColorType::Rgb8)
    };
    res.map_err(|_| ImageErr::Unreadable)?;
    Ok(out)
}

fn read_err(e: image::ImageError) -> ImageErr {
    match e {
        image::ImageError::Limits(_) => ImageErr::TooManyPixels,
        image::ImageError::Unsupported(_) => ImageErr::Unsupported,
        _ => ImageErr::Unreadable,
    }
}

fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// One decode at a time: the container is small, and a decode can take ~70 MB.
fn decode_slot() -> &'static tokio::sync::Semaphore {
    static SLOT: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| tokio::sync::Semaphore::new(1))
}

const ALLOWED: &str = "PNG, JPEG or WebP";

fn unsupported(kind: Kind) -> ApiError {
    ApiError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_type", format!("{} can be {ALLOWED}.", kind.label()))
        .hint("Save it as PNG, JPEG or WebP and try again (GIF, SVG and HEIC aren't accepted).")
}

fn too_large(kind: Kind) -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "too_large",
        format!("{} can be up to {}.", kind.label(), human_bytes(kind.max_bytes())),
    )
    .hint("Make the file smaller (or export it as JPEG) and try again.")
}

fn unreadable() -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "image_unreadable", "That image couldn't be read.")
        .hint("The file may be damaged. Open it, save it again as PNG or JPEG, and upload that.")
}

// ── Routes ───────────────────────────────────────────────────────────────────────────

/// What the dashboard gets.
fn view(st: &AppState, b: &Brand, pro: bool, changes: &[Change]) -> Value {
    let url = |sha: &Option<String>| sha.as_ref().map(|s| format!("{}{}", st.cfg.public_url, asset_path(s)));
    json!({
        "accent": b.colours.map(|c| c.accent.hex()),
        "background": b.colours.map(|c| c.background.hex()),
        "text": b.colours.map(|c| c.text.hex()),
        "avatar_url": url(&b.avatar),
        "banner_url": url(&b.banner),
        // The tip page shows it only while the install is Pro.
        "applied": pro,
        "changes": changes.iter().map(Change::json).collect::<Vec<_>>(),
    })
}

/// `GET /v1/pro/tips/brand`
pub async fn get_brand(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let ctx = require_pro(&st, &headers).await?;
    let b = st.db.tip_brand(&ctx.install).map_err(ApiError::db)?.unwrap_or_default();
    Ok(axum::Json(view(&st, &b, true, &[])).into_response())
}

#[derive(Debug, Deserialize)]
pub struct ColoursBody {
    accent: String,
    background: String,
    text: String,
}

/// `PUT /v1/pro/tips/brand {accent, background, text}`
pub async fn put_brand(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Result<JsonBody<ColoursBody>, ApiError>,
) -> Result<Response, ApiError> {
    // The gate first (401/402), then the body (400/413/415).
    let ctx = require_pro(&st, &headers).await?;
    let JsonBody(b) = body?;
    let field = |name: &str, v: &str| {
        Rgb::parse(v).ok_or_else(|| {
            ApiError::new(StatusCode::BAD_REQUEST, "invalid", format!("{name} must be a hex colour like #3461f5."))
        })
    };
    let picked = Colours {
        accent: field("accent", &b.accent)?,
        background: field("background", &b.background)?,
        text: field("text", &b.text)?,
    };
    let (fixed, changes) = picked.fixed();
    st.db.set_tip_colours(&ctx.install, Some(fixed)).map_err(ApiError::db)?;
    let saved = st.db.tip_brand(&ctx.install).map_err(ApiError::db)?.unwrap_or_default();
    Ok(axum::Json(view(&st, &saved, true, &changes)).into_response())
}

/// `DELETE /v1/pro/tips/brand`: back to the default colours (images stay).
pub async fn delete_brand(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let ctx = require_pro(&st, &headers).await?;
    st.db.set_tip_colours(&ctx.install, None).map_err(ApiError::db)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn kind_of(s: &str) -> Result<Kind, ApiError> {
    Kind::parse(s).ok_or_else(|| {
        ApiError::new(StatusCode::NOT_FOUND, "not_found", "There's no such tip page image.")
            .hint("Use avatar or banner.")
    })
}

/// `PUT /v1/pro/tips/brand/{avatar|banner}`: the raw image as the body.
pub async fn put_image(
    State(st): State<AppState>,
    Path(kind): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let ctx = require_pro(&st, &headers).await?;
    let kind = kind_of(&kind)?;
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let ct = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if !matches!(ct.as_str(), "image/png" | "image/jpeg" | "image/jpg" | "image/webp") {
        return Err(unsupported(kind));
    }
    let max = kind.max_bytes();
    let declared =
        headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > max) {
        drain(body).await;
        return Err(too_large(kind));
    }
    let blobs = st.blobs()?;
    let bytes = read_capped(body, max).await.map_err(|over| if over { too_large(kind) } else { unreadable() })?;
    if bytes.is_empty() {
        return Err(unreadable());
    }
    st.db
        .try_take(&ctx.sub_id, IMAGES_PER_DAY.name, IMAGES_PER_DAY.period, 1, IMAGES_PER_DAY.limit)
        .map_err(|e| e.into_api("tip_image_quota", "That's enough tip page images for today."))?;
    // Counted before the work; a refused or unsaved image gives its upload back.
    let stored = async {
        let webp = {
            let _slot = decode_slot().acquire().await.map_err(|_| crate::blob::unavailable())?;
            tokio::task::spawn_blocking(move || process(kind, &bytes)).await.map_err(|_| unreadable())?
        };
        let webp = match webp {
            Ok(w) => w,
            Err(ImageErr::Unsupported) => return Err(unsupported(kind)),
            Err(ImageErr::TooManyPixels) => {
                return Err(ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "too_large",
                    "That image has too many pixels (4096 × 4096 at most).",
                )
                .hint("Make it smaller in any photo editor and try again."));
            }
            Err(ImageErr::Unreadable) => return Err(unreadable()),
        };
        let sha = sha_hex(&webp);
        let key = blob_key(&sha);
        if blobs.size(&key).await.map_err(crate::blob::BlobError::into_api)?.is_none() {
            blobs.put(&key, Bytes::from(webp)).await.map_err(crate::blob::BlobError::into_api)?;
        }
        Ok::<_, ApiError>(sha)
    }
    .await;
    let sha = match stored {
        Ok(sha) => sha,
        Err(e) => {
            let _ = st.db.give_back(&ctx.sub_id, IMAGES_PER_DAY.name, IMAGES_PER_DAY.period, 1);
            return Err(e);
        }
    };
    let old = st.db.set_tip_image(&ctx.install, kind, Some(&sha)).map_err(ApiError::db)?;
    forget_images(&st, old.into_iter().collect()).await;
    let saved = st.db.tip_brand(&ctx.install).map_err(ApiError::db)?.unwrap_or_default();
    Ok(axum::Json(view(&st, &saved, true, &[])).into_response())
}

/// The body, refusing more than `max` bytes (`Err(true)`) or a broken stream (`Err(false)`).
async fn read_capped(body: Body, max: u64) -> Result<Vec<u8>, bool> {
    let mut stream = body.into_data_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| false)?;
        if out.len() as u64 + chunk.len() as u64 > max {
            drain_stream(stream).await;
            return Err(true);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// `DELETE /v1/pro/tips/brand/{avatar|banner}`
pub async fn delete_image(
    State(st): State<AppState>,
    Path(kind): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let ctx = require_pro(&st, &headers).await?;
    let kind = kind_of(&kind)?;
    let old = st.db.set_tip_image(&ctx.install, kind, None).map_err(ApiError::db)?;
    forget_images(&st, old.into_iter().collect()).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `GET /t/a/{sha}.webp` (relay and tip host): a stored image, cached for good (its name
/// is its hash).
pub async fn asset(State(st): State<AppState>, Path(file): Path<String>) -> Response {
    let sha = file.strip_suffix(".webp").unwrap_or("");
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(blobs) = st.blobs() else {
        return crate::blob::unavailable().into_response();
    };
    match blobs.get(&blob_key(sha)).await {
        Ok(stream) => {
            let mut res = Body::from_stream(stream).into_response();
            let h = res.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/webp"));
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable"));
            h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
            h.insert("content-security-policy", HeaderValue::from_static("default-src 'none'; sandbox"));
            res
        }
        Err(crate::blob::BlobError::NotFound | crate::blob::BlobError::BadKey) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => e.into_api().into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_colours_parse_and_print() {
        assert_eq!(Rgb::parse("#3461F5"), Some(Rgb(0x34, 0x61, 0xf5)));
        assert_eq!(Rgb::parse(" #fff "), Some(Rgb(255, 255, 255)));
        assert_eq!(Rgb::parse("#0c0e12").unwrap().hex(), "#0c0e12");
        for bad in ["", "#", "fff", "#ffff", "#ggg000", "#12345", "#1234567", "red", "#+12345", "#ffé"] {
            assert_eq!(Rgb::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn orange_buttons_get_white_text_on_a_dark_enough_fill() {
        for hex in ["#ffa424", "#ff5a1f", "#cf7400", "#f97316", "#b35900"] {
            let (fill, on) = button_colours(Rgb::parse(hex).unwrap());
            assert_eq!(on, WHITE, "{hex}");
            assert!(contrast(WHITE, fill) >= TEXT_CONTRAST, "{hex} -> {}", fill.hex());
        }
        assert_eq!(button_colours(Rgb::parse("#b35900").unwrap()).0.hex(), "#b35900", "already dark enough");
        let yellow = Rgb::parse("#ffe14d").unwrap();
        assert_eq!(button_colours(yellow), (yellow, BLACK), "not orange: unchanged");
    }

    #[test]
    fn contrast_matches_wcag() {
        assert!((contrast(WHITE, BLACK) - 21.0).abs() < 1e-9);
        assert!((contrast(Rgb(0x77, 0x77, 0x77), WHITE) - 4.48).abs() < 0.01);
        assert!((contrast(Rgb(1, 2, 3), Rgb(1, 2, 3)) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn unreadable_text_is_moved_until_it_reads_and_says_so() {
        // Grey text on a mid-grey background: under 4.5:1.
        let picked =
            Colours { accent: Rgb(0x80, 0x80, 0x80), background: Rgb(0x60, 0x60, 0x60), text: Rgb(0x90, 0x90, 0x90) };
        let (fixed, changes) = picked.fixed();
        assert!(contrast(fixed.text, picked.background) >= TEXT_CONTRAST);
        assert!(contrast(fixed.accent, picked.background) >= ACCENT_CONTRAST);
        assert_eq!(fixed.background, picked.background, "the background is never changed");
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].field, "text");
        let j = changes[0].json();
        assert!(j["message"].as_str().unwrap().contains("4.5:1"), "{j}");
        assert!(j["contrast_before"].as_f64().unwrap() < 4.5 && j["contrast_after"].as_f64().unwrap() >= 4.5);
        // Readable colours stay exactly as picked.
        let good =
            Colours { accent: Rgb(0xff, 0xa4, 0x24), background: Rgb(0x0a, 0x0b, 0x0d), text: Rgb(0xee, 0xf0, 0xf4) };
        assert_eq!(good.fixed(), (good, vec![]));
        // Every background can reach 4.5:1 with black or white.
        for v in (0..=255).step_by(5) {
            let bg = Rgb(v, v, v);
            assert!(contrast(readable(bg, bg, TEXT_CONTRAST), bg) >= TEXT_CONTRAST, "{v}");
        }
    }

    #[test]
    fn a_light_background_makes_a_light_page() {
        let p = Palette::from_colours(Colours {
            accent: Rgb(0xcf, 0x74, 0x00),
            background: Rgb(0xf6, 0xf1, 0xe7),
            text: Rgb(0x1a, 0x14, 0x0c),
        });
        assert_eq!(p.scheme, "light");
        let (bg, fg, muted) = (Rgb::parse(&p.bg).unwrap(), Rgb::parse(&p.fg).unwrap(), Rgb::parse(&p.muted).unwrap());
        assert!(contrast(fg, bg) >= TEXT_CONTRAST && contrast(muted, bg) >= TEXT_CONTRAST);
        let on = Rgb::parse(&p.on_accent).unwrap();
        assert_eq!(on, WHITE, "text on orange is white");
        assert!(contrast(on, Rgb::parse(&p.accent_fill).unwrap()) >= TEXT_CONTRAST);
        assert_eq!(Palette::default().scheme, "dark");
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 90]));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn images_are_shrunk_and_reencoded_as_webp() {
        let out = process(Kind::Banner, &png(3000, 600)).unwrap();
        assert_eq!(image::guess_format(&out).unwrap(), image::ImageFormat::WebP);
        let back = image::load_from_memory(&out).unwrap();
        assert_eq!((back.width(), back.height()), (1500, 300), "fits 1500 × 500, aspect kept");
        let small = process(Kind::Avatar, &png(200, 100)).unwrap();
        let back = image::load_from_memory(&small).unwrap();
        assert_eq!((back.width(), back.height()), (200, 100), "small images aren't blown up");
        let big = process(Kind::Avatar, &png(1024, 2048)).unwrap();
        let back = image::load_from_memory(&big).unwrap();
        assert_eq!((back.width(), back.height()), (256, 512));
    }

    #[test]
    fn jpeg_metadata_is_dropped_and_sideways_photos_turn_upright() {
        // A 40 × 20 JPEG with an EXIF block saying "rotate 90°" and a fake GPS tag.
        let img = image::RgbImage::from_fn(40, 20, |_, _| image::Rgb([200, 30, 30]));
        let mut jpg = Cursor::new(Vec::new());
        img.write_to(&mut jpg, image::ImageFormat::Jpeg).unwrap();
        let jpg = jpg.into_inner();
        let exif = exif_orientation_6();
        let mut with_exif = jpg[..2].to_vec(); // SOI
        with_exif.extend_from_slice(&[0xff, 0xe1]);
        with_exif.extend_from_slice(&u16::try_from(exif.len() + 2).unwrap().to_be_bytes());
        with_exif.extend_from_slice(&exif);
        with_exif.extend_from_slice(&jpg[2..]);
        let out = process(Kind::Avatar, &with_exif).unwrap();
        let back = image::load_from_memory(&out).unwrap();
        assert_eq!((back.width(), back.height()), (20, 40), "turned upright");
        assert!(!out.windows(4).any(|w| w == b"Exif"), "no EXIF in the stored file");
        assert!(!out.windows(8).any(|w| w == b"GPSFAKE!"), "nothing from the original's metadata");
    }

    /// `Exif\0\0` + a little-endian TIFF with Orientation = 6 and an ASCII tag holding a marker.
    fn exif_orientation_6() -> Vec<u8> {
        let mut t = b"Exif\0\0II*\0".to_vec();
        t.extend_from_slice(&8u32.to_le_bytes()); // IFD at 8
        t.extend_from_slice(&2u16.to_le_bytes()); // 2 entries
        t.extend_from_slice(&[0x12, 0x01, 3, 0]); // Orientation, SHORT
        t.extend_from_slice(&1u32.to_le_bytes());
        t.extend_from_slice(&[6, 0, 0, 0]);
        t.extend_from_slice(&[0x0e, 0x01, 2, 0]); // ImageDescription, ASCII
        t.extend_from_slice(&9u32.to_le_bytes());
        t.extend_from_slice(&38u32.to_le_bytes()); // offset of the string
        t.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
        t.extend_from_slice(b"GPSFAKE!\0");
        t
    }

    #[test]
    fn other_files_and_broken_images_are_refused() {
        assert_eq!(process(Kind::Avatar, b"GIF89a\x01\x00\x01\x00"), Err(ImageErr::Unsupported));
        assert_eq!(process(Kind::Avatar, b"<svg xmlns='http://www.w3.org/2000/svg'/>"), Err(ImageErr::Unsupported));
        let mut broken = png(64, 64);
        broken.truncate(60);
        assert_eq!(process(Kind::Avatar, &broken), Err(ImageErr::Unreadable));
        // A PNG header claiming 20000 × 20000 is refused before any decoding.
        let mut huge = png(1, 1);
        huge[16..20].copy_from_slice(&20_000u32.to_be_bytes());
        huge[20..24].copy_from_slice(&20_000u32.to_be_bytes());
        assert!(matches!(process(Kind::Banner, &huge), Err(ImageErr::TooManyPixels | ImageErr::Unreadable)));
    }
}
