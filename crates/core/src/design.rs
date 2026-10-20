//! Designs: alert looks, full overlay themes, single scenes and widget styles, made in the
//! dashboard's designer or shipped with the app (API.md §4 Designs).
//!
//! Built-in designs come with the app, carry a small Ferrocast mark (`branded`) and can't be
//! edited; "Duplicate & customise" makes the streamer's own copy without any branding. A
//! design is plain data: no scripts, no tokens, nothing about viewers, so it can be shared as
//! a `.ferrodesign` file. Everything here is pure, so the overlay preview in the browser can
//! port it one to one.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::alert::{Condition, MAX_DURATION_MS, MIN_DURATION_MS, RenderedAlert, amount_of};
use crate::event::{Event, EventKind, Platform};
use crate::layer::{self, Check, check_css, color, font_family, media_ref};
use crate::money::ValueUnit;
use crate::overlay::{Overlay, Widget, WidgetKind};
use crate::template::{Template, TextPart, event_vars, with_plurals};

/// The design format this build reads and writes. A newer one is refused with "update
/// Ferrocast", never half-read.
pub const SCHEMA: u32 = 1;
pub const MAX_NAME: usize = 60;
pub const MAX_DESCRIPTION: usize = 200;
pub const MAX_VARIATIONS: usize = 10;
pub const MAX_SCENES: usize = 8;
pub const MAX_TITLE: usize = 300;
pub const MAX_BODY: usize = 500;
/// The alert canvas the designer and the overlay share (offsets and sizes are in it).
pub const ALERT_CANVAS: (u32, u32) = (800, 450);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesignKind {
    Alert,
    Theme,
    Scene,
    Widget,
}

impl DesignKind {
    pub fn key(self) -> &'static str {
        match self {
            DesignKind::Alert => "alert",
            DesignKind::Theme => "theme",
            DesignKind::Scene => "scene",
            DesignKind::Widget => "widget",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StyleTag {
    Minimal,
    Bold,
    Retro,
    Playful,
    Tech,
    Cozy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MotionTag {
    Still,
    Smooth,
    Lively,
    Bouncy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tags {
    pub style: StyleTag,
    pub motion: MotionTag,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginKind {
    BuiltIn,
    Scratch,
    Duplicate,
    Import,
    AssetPack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    #[serde(rename = "type")]
    pub kind: OriginKind,
    /// What it was made from: the template's name, the file's name.
    #[serde(default)]
    pub from: Option<String>,
}

/// One design. `kind` and `content` travel side by side (`{ "kind": "alert", "content": … }`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Design {
    pub schema: u32,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub tags: Tags,
    /// Built-ins only: part of the paid catalogue. Never set on the streamer's own designs.
    #[serde(default)]
    pub pro: bool,
    #[serde(default)]
    pub built_in: bool,
    /// Shows the small Ferrocast mark. Only built-ins are branded.
    #[serde(default)]
    pub branded: bool,
    pub origin: Origin,
    #[serde(flatten)]
    pub content: DesignContent,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "content", rename_all = "snake_case")]
pub enum DesignContent {
    Alert(AlertContent),
    Theme(ThemeContent),
    Scene(Scene),
    Widget(WidgetContent),
}

impl DesignContent {
    pub fn kind(&self) -> DesignKind {
        match self {
            DesignContent::Alert(_) => DesignKind::Alert,
            DesignContent::Theme(_) => DesignKind::Theme,
            DesignContent::Scene(_) => DesignKind::Scene,
            DesignContent::Widget(_) => DesignKind::Widget,
        }
    }
}

// ------------------------------------------------------------------------------- alerts

/// Which look an event uses. Finer than event kinds: a resub, a YouTube membership and a
/// Kick gift of KICKs can each look different, and fall back to the plainer look.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKey {
    Follow,
    Sub,
    Resub,
    Gift,
    Member,
    Bits,
    Kicks,
    Tip,
    Superchat,
    Raid,
    Redeem,
}

pub const EVENT_KEYS: [EventKey; 11] = [
    EventKey::Follow,
    EventKey::Sub,
    EventKey::Resub,
    EventKey::Gift,
    EventKey::Member,
    EventKey::Bits,
    EventKey::Kicks,
    EventKey::Tip,
    EventKey::Superchat,
    EventKey::Raid,
    EventKey::Redeem,
];

impl EventKey {
    pub fn key(self) -> &'static str {
        match self {
            EventKey::Follow => "follow",
            EventKey::Sub => "sub",
            EventKey::Resub => "resub",
            EventKey::Gift => "gift",
            EventKey::Member => "member",
            EventKey::Bits => "bits",
            EventKey::Kicks => "kicks",
            EventKey::Tip => "tip",
            EventKey::Superchat => "superchat",
            EventKey::Raid => "raid",
            EventKey::Redeem => "redeem",
        }
    }

    /// Looks tried in order: its own, then the plainer one it resembles.
    pub fn fallbacks(self) -> &'static [EventKey] {
        match self {
            EventKey::Resub => &[EventKey::Resub, EventKey::Sub],
            EventKey::Member => &[EventKey::Member, EventKey::Sub],
            EventKey::Gift => &[EventKey::Gift, EventKey::Sub],
            EventKey::Kicks => &[EventKey::Kicks, EventKey::Bits],
            EventKey::Superchat => &[EventKey::Superchat, EventKey::Tip],
            EventKey::Follow => &[EventKey::Follow],
            EventKey::Sub => &[EventKey::Sub],
            EventKey::Bits => &[EventKey::Bits],
            EventKey::Tip => &[EventKey::Tip],
            EventKey::Raid => &[EventKey::Raid],
            EventKey::Redeem => &[EventKey::Redeem],
        }
    }
}

/// The look key for an event, `None` for events that never alert.
pub fn event_key(event: &Event) -> Option<EventKey> {
    Some(match &event.kind {
        EventKind::Follow { .. } => EventKey::Follow,
        EventKind::Subscription { .. } if event.platform == Platform::Youtube => EventKey::Member,
        EventKind::Subscription { months, .. } if *months >= 2 => EventKey::Resub,
        EventKind::Subscription { .. } => EventKey::Sub,
        EventKind::GiftBatch { .. } => EventKey::Gift,
        EventKind::PlatformGift { value, .. } if matches!(value.unit, ValueUnit::Kicks) => EventKey::Kicks,
        EventKind::PlatformGift { .. } => EventKey::Bits,
        EventKind::PaidMessage { .. } => EventKey::Superchat,
        EventKind::Tip { .. } => EventKey::Tip,
        EventKind::Raid { .. } => EventKey::Raid,
        EventKind::RewardRedemption { .. } => EventKey::Redeem,
        _ => return None,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertContent {
    pub events: BTreeMap<EventKey, EventLooks>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventLooks {
    pub variations: Vec<DesignVariation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DesignVariation {
    pub id: String,
    pub name: String,
    /// `None` is the default look for the event; exactly one per event.
    pub condition: Option<Condition>,
    /// Higher wins when several variations match.
    #[serde(default)]
    pub priority: i32,
    pub look: AlertLook,
}

/// The decoration around the text: each one is a different structure, not a recolour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Frame {
    /// Dark plate with a molten top edge.
    Plate,
    /// A glowing tube outline.
    Neon,
    /// A starburst behind tilted, inked text.
    Comic,
    /// Frosted panel.
    Glass,
    /// Stepped pixel border, 8-bit.
    Pixel,
    /// Slanted full-width bar with a tag.
    Banner,
    /// One slim line.
    Pill,
    /// Text on its own.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    Above,
    Left,
    Right,
    Over,
    Text,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    None,
    /// The event's icon drawn in the look's colours.
    Emblem,
    Image,
    Video,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaSlot {
    pub kind: MediaKind,
    /// A media id from the library, or `asset:<path>` for a file bundled with the app.
    #[serde(rename = "ref", default)]
    pub media_ref: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Offsets {
    pub text_x: i32,
    pub text_y: i32,
    pub media_x: i32,
    pub media_y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FontSource {
    /// Shipped with Ferrocast (Bricolage Grotesque, Hanken Grotesk, JetBrains Mono).
    Bundled,
    /// A Google Fonts family (open licence).
    Google,
    /// Installed on the computer.
    System,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Font {
    pub family: String,
    pub source: FontSource,
    pub size: u32,
    pub weight: u32,
    #[serde(default)]
    pub uppercase: bool,
    #[serde(default)]
    pub italic: bool,
    /// In em.
    #[serde(default)]
    pub letter_spacing: f32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Colors {
    pub text: String,
    /// The name and amount.
    pub highlight: String,
    pub plate: String,
    pub accent: String,
    pub outline: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shadow {
    None,
    Soft,
    Hard,
    Glow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnimIn {
    None,
    Fade,
    Rise,
    Drop,
    SlideLeft,
    SlideRight,
    Zoom,
    Pop,
    Bounce,
    Flip,
    Wipe,
    Magnet,
    // The motion designer's entrances (1.1): springs, impacts and particles, tuned by `Motion`.
    Slam,
    Elastic,
    Bolt,
    Glitch,
    Portal,
    Shatter,
    Pour,
    FlipShine,
    ConfettiPop,
    Dash,
    Neon,
}

/// The entrances a scene layer can use (`config.anim.in`): the simple ones only. Alert looks
/// also take the motion designer's (`AnimIn`).
pub const ANIM_IN_NAMES: [&str; 12] =
    ["none", "fade", "rise", "drop", "slide_left", "slide_right", "zoom", "pop", "bounce", "flip", "wipe", "magnet"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnimOut {
    None,
    Fade,
    Sink,
    Lift,
    SlideLeft,
    SlideRight,
    Zoom,
    Shrink,
    Flip,
    Wipe,
    // The motion designer's exits (1.1).
    Dissolve,
    BlackHole,
    Zip,
    Fold,
    Shatter,
    TvOff,
    Fall,
    Iris,
    Glitch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextAnim {
    None,
    Wave,
    Wiggle,
    Bounce,
    Pop,
    /// Typewriter: the letters type out behind a cursor (1.0's "type", now with the cursor).
    Type,
    Glow,
    // The motion designer's text effects (1.1).
    Cascade,
    PopWave,
    Decode,
    Shine,
    Neon,
    Rgb,
    Stretch,
    Spin,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Anim {
    #[serde(rename = "in")]
    pub enter: AnimIn,
    #[serde(rename = "out")]
    pub exit: AnimOut,
    pub text: TextAnim,
    pub in_ms: u32,
    pub out_ms: u32,
    /// The text appears this long after the alert starts coming in.
    #[serde(default)]
    pub text_delay_ms: u32,
}

/// What the alert does while it's on screen, between its entrance and its exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Idle {
    #[default]
    None,
    Breathe,
    Float,
    Shimmer,
    Glow,
    Jelly,
    Heartbeat,
}

/// The particles that fly when the alert lands. `Auto` uses the entrance's own (the simple
/// entrances have none).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Particles {
    #[default]
    Auto,
    None,
    Confetti,
    Sparks,
    Petals,
    Coins,
    Embers,
    Stars,
}

/// The particles' colours. `Brand` takes the look's own colours; `Custom` uses `custom_color`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticleColors {
    #[default]
    Brand,
    Rainbow,
    Gold,
    White,
    Custom,
}

/// Where the card sits on the alert canvas: rows top, middle, bottom × columns left, centre,
/// right. `mc` is the middle, where every 1.0 alert sat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Spot {
    Tl,
    Tc,
    Tr,
    Ml,
    #[default]
    Mc,
    Mr,
    Bl,
    Bc,
    Br,
}

/// The side a directional entrance (Elastic drop, Dash in, Flip and shine) or the Slide and
/// blur exit uses. `Auto` lets each one pick its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    #[default]
    Auto,
    Top,
    Left,
    Right,
    Bottom,
}

/// The recipe the motion came from, so the designer can show it; any change clears it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recipe {
    Heavy,
    Arcade,
    Cyber,
    Molten,
    Storm,
    Party,
    Clean,
}

pub const MOTION_BOUNCE_MAX: u8 = 100;
pub const MOTION_SPEED: (u16, u16) = (60, 160);
pub const MOTION_STAGGER_MAX: u16 = 90;
pub const MOTION_BPM: (u16, u16) = (70, 170);

/// The motion designer's settings for a look (1.1). Looks saved before it have none and read
/// as `Motion::default()`, which changes nothing about how they play: no idle loop, the card
/// in the middle, and the 1.0 entrances carry no impacts or particles. Every field has a
/// default, so a partial object from a newer build still reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Motion {
    pub idle: Idle,
    pub particles: Particles,
    pub colors: ParticleColors,
    pub custom_color: String,
    /// Spring bounce, 0 (firm) to 100 (springy).
    pub bounce: u8,
    /// Playback speed in percent, 60 (relaxed) to 160 (snappy).
    pub speed: u16,
    /// Milliseconds between letters for the per-letter text effects, 0 to 90.
    pub stagger: u16,
    pub shake: bool,
    pub flash: bool,
    pub vignette: bool,
    pub zoom_punch: bool,
    /// Bigger amounts shake harder, throw more particles and hit louder (`intensity`).
    pub by_amount: bool,
    pub position: Spot,
    pub direction: Direction,
    /// Short synthesised hits on each landing, at the look's sound volume. Off by default.
    pub impact_sounds: bool,
    /// Start a little later so the entrance's hit lands on a downbeat of the alert's sound.
    pub beat: bool,
    pub bpm: u16,
    pub recipe: Option<Recipe>,
}

impl Default for Motion {
    fn default() -> Self {
        Motion {
            idle: Idle::None,
            particles: Particles::Auto,
            colors: ParticleColors::Brand,
            custom_color: "#ff5aa5".into(),
            bounce: 45,
            speed: 100,
            stagger: 34,
            shake: true,
            flash: true,
            vignette: true,
            zoom_punch: true,
            by_amount: true,
            position: Spot::Mc,
            direction: Direction::Auto,
            impact_sounds: false,
            beat: false,
            bpm: 120,
            recipe: None,
        }
    }
}

impl Motion {
    fn validate(&self, path: &str) -> Check {
        color(&format!("{path}.custom_color"), &self.custom_color)?;
        within(&format!("{path}.bounce"), u32::from(self.bounce), 0, u32::from(MOTION_BOUNCE_MAX), "")?;
        within(&format!("{path}.speed"), u32::from(self.speed), MOTION_SPEED.0.into(), MOTION_SPEED.1.into(), "%")?;
        within(&format!("{path}.stagger"), u32::from(self.stagger), 0, u32::from(MOTION_STAGGER_MAX), " ms")?;
        within(&format!("{path}.bpm"), u32::from(self.bpm), MOTION_BPM.0.into(), MOTION_BPM.1.into(), " BPM")
    }
}

/// The overlay's built-in alert sounds (files in the overlay bundle). Chime, Coin, Whoosh,
/// Boing and Bass retired in 1.1 but stay valid: saved designs keep their id and the overlay
/// plays the closest new sound (Glint, Tally, Lift, Droplet, Ember).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinSound {
    None,
    // Retired in 1.1, kept so saved designs load.
    Chime,
    Coin,
    Whoosh,
    Boing,
    Bass,
    // Crystal
    Sparkle,
    Glint,
    Cascade,
    Bloom,
    Lift,
    Tally,
    Halo,
    Droplet,
    // Felt
    Kalimba,
    Marimba,
    Glow,
    Ember,
    Pebble,
    Feather,
    Hearth,
    Wink,
    /// The pre-1.1 Sparkle.
    SparkleClassic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sound {
    /// A sound from the media library; plays instead of `builtin`.
    #[serde(rename = "ref", default)]
    pub media_ref: Option<String>,
    pub builtin: BuiltinSound,
    pub volume: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Tts {
    pub enabled: bool,
    /// Major units; 0 reads every message. Events without an amount aren't limited.
    pub min_amount: f64,
}

/// Where the small Spark mark sits on a branded design.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrandSpot {
    #[default]
    Corner,
    Divider,
    Watermark,
    Notch,
    Badge,
    Edge,
}

pub const BRAND_SPOT_NAMES: [&str; 6] = ["corner", "divider", "watermark", "notch", "badge", "edge"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertLook {
    pub frame: Frame,
    pub layout: Layout,
    pub media: MediaSlot,
    pub media_size: u32,
    #[serde(default)]
    pub offsets: Offsets,
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub font: Font,
    pub colors: Colors,
    pub plate: bool,
    pub shadow: Shadow,
    #[serde(default)]
    pub outline: f32,
    pub anim: Anim,
    /// Missing on looks saved before 1.1: they read as the default, which plays as before.
    #[serde(default)]
    pub motion: Motion,
    pub sound: Sound,
    pub duration_ms: u32,
    #[serde(default)]
    pub tts: Tts,
    #[serde(default)]
    pub brand: BrandSpot,
    #[serde(default)]
    pub css: String,
}

// ------------------------------------------------------------------------------- themes

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThemeContent {
    pub scenes: Vec<Scene>,
    /// The alert design that goes with the theme (used when "Use" also takes its alerts).
    #[serde(default)]
    pub alert_design: Option<String>,
}

/// The standard scene keys; `custom_<n>` are allowed too.
pub const SCENE_KEYS: [&str; 5] = ["in_game", "just_chatting", "starting_soon", "brb", "ending"];

/// One screen: layers are overlay widgets, bottom to top, so "Use" turns a scene into an
/// overlay as is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    pub key: String,
    pub name: String,
    pub width: i32,
    pub height: i32,
    #[serde(default)]
    pub background: Option<String>,
    pub layers: Vec<Widget>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WidgetContent {
    pub widget: WidgetKind,
    pub w: i32,
    pub h: i32,
    /// Starting settings for the widget (its `config`, without the style).
    #[serde(default = "empty_config")]
    pub config: serde_json::Value,
    /// How it looks; copied into `config.style` when the widget is added.
    pub style: serde_json::Value,
}

fn empty_config() -> serde_json::Value {
    serde_json::json!({})
}

// ------------------------------------------------------------------------------- checks

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DesignError {
    /// A field is wrong; the message names it and says what to do.
    #[error("{0}")]
    Invalid(String),
    /// Made by a newer Ferrocast.
    #[error("This design was made by a newer Ferrocast (design format {0}). Update Ferrocast, then try again.")]
    TooNew(u32),
}

fn invalid(m: impl Into<String>) -> DesignError {
    DesignError::Invalid(m.into())
}

fn within<T: PartialOrd + std::fmt::Display + Copy>(path: &str, v: T, lo: T, hi: T, unit: &str) -> Check {
    if v >= lo && v <= hi { Ok(()) } else { Err(format!("{path}: must be {lo} to {hi}{unit}.")) }
}

fn within_f(path: &str, v: f32, lo: f32, hi: f32) -> Check {
    if v.is_finite() && v >= lo && v <= hi { Ok(()) } else { Err(format!("{path}: must be {lo} to {hi}.")) }
}

fn is_slug(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn template(path: &str, src: &str, max: usize) -> Check {
    if src.chars().count() > max {
        return Err(format!("{path}: can be up to {max} characters."));
    }
    Template::parse(src).map(|_| ()).map_err(|e| format!("{path}: {e}."))
}

impl Design {
    /// Checks everything the editor and importers must refuse. Messages name the field.
    pub fn validate(&self) -> Result<(), DesignError> {
        if self.schema > SCHEMA {
            return Err(DesignError::TooNew(self.schema));
        }
        if self.schema == 0 {
            return Err(invalid("schema: must be 1."));
        }
        if !is_slug(&self.id, 80) {
            return Err(invalid("id: use letters, digits, dots, dashes or underscores (up to 80)."));
        }
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > MAX_NAME {
            return Err(invalid(format!("name: give the design a name of 1 to {MAX_NAME} characters.")));
        }
        if self.description.chars().count() > MAX_DESCRIPTION {
            return Err(invalid(format!("description: can be up to {MAX_DESCRIPTION} characters.")));
        }
        if let Some(from) = &self.origin.from
            && from.chars().count() > 120
        {
            return Err(invalid("origin.from: can be up to 120 characters."));
        }
        if self.branded && !self.built_in {
            return Err(invalid("branded: only built-in designs carry the Ferrocast mark."));
        }
        if !self.branded && self.has_brand_layer() {
            return Err(invalid("content: the Ferrocast mark layer is only in built-in designs; remove it."));
        }
        match &self.content {
            DesignContent::Alert(a) => a.validate().map_err(invalid),
            DesignContent::Theme(t) => t.validate().map_err(invalid),
            DesignContent::Scene(s) => s.validate("content").map_err(invalid),
            DesignContent::Widget(w) => w.validate().map_err(invalid),
        }
    }

    pub fn kind(&self) -> DesignKind {
        self.content.kind()
    }

    /// The streamer's own copy: never built-in, never branded, never Pro, a fresh id. Every
    /// design the API creates passes through here, so the branding rule can't be skipped.
    pub fn into_user(mut self, id: String, origin: Origin, now: i64) -> Design {
        self.id = id;
        self.built_in = false;
        self.branded = false;
        self.pro = false;
        self.origin = origin;
        self.created_at = now;
        self.updated_at = now;
        self.schema = SCHEMA;
        self.strip_brand_layers();
        self
    }

    /// Removes the Spark mark layers a built-in theme or scene carries.
    pub fn strip_brand_layers(&mut self) {
        let keep = |l: &Widget| !is_brand_layer(l);
        match &mut self.content {
            DesignContent::Theme(t) => t.scenes.iter_mut().for_each(|s| s.layers.retain(keep)),
            DesignContent::Scene(s) => s.layers.retain(keep),
            DesignContent::Alert(_) | DesignContent::Widget(_) => {}
        }
        if let DesignContent::Widget(w) = &mut self.content
            && let Some(obj) = w.style.as_object_mut()
        {
            obj.remove("brand");
        }
    }

    fn has_brand_layer(&self) -> bool {
        match &self.content {
            DesignContent::Theme(t) => t.scenes.iter().flat_map(|s| &s.layers).any(is_brand_layer),
            DesignContent::Scene(s) => s.layers.iter().any(is_brand_layer),
            DesignContent::Alert(_) | DesignContent::Widget(_) => false,
        }
    }

    /// Every media id from the library this design uses (not bundled `asset:` files).
    pub fn media_ids(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut push = |r: Option<&str>| {
            if let Some(r) = r
                && !r.starts_with("asset:")
                && !out.iter().any(|x: &String| x == r)
            {
                out.push(r.to_string());
            }
        };
        match &self.content {
            DesignContent::Alert(a) => {
                for v in a.events.values().flat_map(|e| &e.variations) {
                    push(v.look.media.media_ref.as_deref());
                    push(v.look.sound.media_ref.as_deref());
                }
            }
            DesignContent::Theme(t) => {
                for s in &t.scenes {
                    for l in &s.layers {
                        push(l.config.get("media").and_then(serde_json::Value::as_str));
                    }
                }
            }
            DesignContent::Scene(s) => {
                for l in &s.layers {
                    push(l.config.get("media").and_then(serde_json::Value::as_str));
                }
            }
            DesignContent::Widget(_) => {}
        }
        out
    }

    /// Replaces media ids (after an import copied the files under new ids). Ids not in
    /// `map` are left alone.
    pub fn remap_media(&mut self, map: &BTreeMap<String, String>) {
        let swap = |r: &mut Option<String>| {
            if let Some(new) = r.as_ref().and_then(|old| map.get(old)) {
                *r = Some(new.clone());
            }
        };
        let swap_layer = |l: &mut Widget| {
            let new = l.config.get("media").and_then(serde_json::Value::as_str).and_then(|old| map.get(old)).cloned();
            if let (Some(new), Some(obj)) = (new, l.config.as_object_mut()) {
                obj.insert("media".into(), serde_json::Value::String(new));
            }
        };
        match &mut self.content {
            DesignContent::Alert(a) => {
                for v in a.events.values_mut().flat_map(|e| &mut e.variations) {
                    swap(&mut v.look.media.media_ref);
                    swap(&mut v.look.sound.media_ref);
                }
            }
            DesignContent::Theme(t) => t.scenes.iter_mut().flat_map(|s| &mut s.layers).for_each(swap_layer),
            DesignContent::Scene(s) => s.layers.iter_mut().for_each(swap_layer),
            DesignContent::Widget(_) => {}
        }
    }
}

impl AlertContent {
    fn validate(&self) -> Check {
        if self.events.is_empty() {
            return Err("content.events: add a look for at least one event.".into());
        }
        for (key, looks) in &self.events {
            let base = format!("content.events.{}", key.key());
            if looks.variations.is_empty() || looks.variations.len() > MAX_VARIATIONS {
                return Err(format!("{base}: needs 1 to {MAX_VARIATIONS} looks."));
            }
            let defaults = looks.variations.iter().filter(|v| v.condition.is_none()).count();
            if defaults != 1 {
                return Err(format!("{base}: needs exactly one default look (a look without a condition)."));
            }
            let mut ids = std::collections::HashSet::new();
            for (i, v) in looks.variations.iter().enumerate() {
                let path = format!("{base}.variations[{i}]");
                if !is_slug(&v.id, 64) {
                    return Err(format!("{path}.id: use letters, digits, dots, dashes or underscores."));
                }
                if !ids.insert(v.id.as_str()) {
                    return Err(format!("{path}.id: two looks share the id \"{}\".", v.id));
                }
                let n = v.name.trim().chars().count();
                if n == 0 || n > 40 {
                    return Err(format!("{path}.name: give the look a name of 1 to 40 characters."));
                }
                if let Some(c) = &v.condition {
                    check_condition(&format!("{path}.condition"), c, 0)?;
                }
                v.look.validate(&format!("{path}.look"))?;
            }
        }
        Ok(())
    }
}

fn check_condition(path: &str, c: &Condition, depth: usize) -> Check {
    if depth > 4 {
        return Err(format!("{path}: conditions can be nested at most 4 deep."));
    }
    match c {
        Condition::AmountAtLeast { value, currency } | Condition::AmountBelow { value, currency } => {
            if !value.is_finite() || *value < 0.0 || *value > 1e12 {
                return Err(format!("{path}.value: must be a number from 0 up."));
            }
            if let Some(cur) = currency
                && !(cur.len() == 3 && cur.bytes().all(|b| b.is_ascii_alphabetic()))
            {
                return Err(format!("{path}.currency: use a 3-letter currency code like INR."));
            }
        }
        Condition::TierAtLeast { tier } => within(&format!("{path}.tier"), *tier, 1, 3, "")?,
        Condition::MonthsAtLeast { months } => within(&format!("{path}.months"), *months, 0, 1200, " months")?,
        Condition::MessageContains { text } => {
            if text.chars().count() > 100 {
                return Err(format!("{path}.text: can be up to 100 characters."));
            }
        }
        Condition::All { of } | Condition::Any { of } => {
            if of.is_empty() || of.len() > 10 {
                return Err(format!("{path}.of: combine 1 to 10 conditions."));
            }
            for (i, inner) in of.iter().enumerate() {
                check_condition(&format!("{path}.of[{i}]"), inner, depth + 1)?;
            }
        }
        Condition::PlatformIs { .. } | Condition::IsGifted | Condition::HasMessage => {}
    }
    Ok(())
}

impl AlertLook {
    fn validate(&self, path: &str) -> Check {
        within(&format!("{path}.media_size"), self.media_size, 24, 480, " px")?;
        match self.media.kind {
            MediaKind::Image | MediaKind::Video => {
                let r = self
                    .media
                    .media_ref
                    .as_deref()
                    .ok_or_else(|| format!("{path}.media.ref: pick the picture or video to show."))?;
                media_ref(&format!("{path}.media.ref"), r)?;
            }
            MediaKind::None | MediaKind::Emblem => {
                if let Some(r) = &self.media.media_ref {
                    media_ref(&format!("{path}.media.ref"), r)?;
                }
            }
        }
        let o = &self.offsets;
        for (name, v) in [("text_x", o.text_x), ("text_y", o.text_y), ("media_x", o.media_x), ("media_y", o.media_y)] {
            within(&format!("{path}.offsets.{name}"), v, -400, 400, " px")?;
        }
        template(&format!("{path}.title"), &self.title, MAX_TITLE)?;
        template(&format!("{path}.body"), &self.body, MAX_BODY)?;
        font_family(&format!("{path}.font.family"), &self.font.family)?;
        within(&format!("{path}.font.size"), self.font.size, 10, 160, " px")?;
        within(&format!("{path}.font.weight"), self.font.weight, 100, 900, "")?;
        within_f(&format!("{path}.font.letter_spacing"), self.font.letter_spacing, -0.2, 0.5)?;
        let c = &self.colors;
        for (name, v) in [
            ("text", &c.text),
            ("highlight", &c.highlight),
            ("plate", &c.plate),
            ("accent", &c.accent),
            ("outline", &c.outline),
        ] {
            color(&format!("{path}.colors.{name}"), v)?;
        }
        within_f(&format!("{path}.outline"), self.outline, 0.0, 12.0)?;
        within(&format!("{path}.anim.in_ms"), self.anim.in_ms, 0, 3000, " ms")?;
        within(&format!("{path}.anim.out_ms"), self.anim.out_ms, 0, 3000, " ms")?;
        within(&format!("{path}.anim.text_delay_ms"), self.anim.text_delay_ms, 0, 10_000, " ms")?;
        self.motion.validate(&format!("{path}.motion"))?;
        if let Some(r) = &self.sound.media_ref {
            media_ref(&format!("{path}.sound.ref"), r)?;
        }
        within(&format!("{path}.sound.volume"), self.sound.volume, 0, 100, "")?;
        if !(MIN_DURATION_MS..=MAX_DURATION_MS).contains(&self.duration_ms) {
            return Err(format!("{path}.duration_ms: an alert stays 1 to 60 seconds."));
        }
        if self.anim.in_ms.saturating_add(self.anim.out_ms) >= self.duration_ms {
            return Err(format!("{path}.duration_ms: leave time to read it between coming in and going out."));
        }
        if !self.tts.min_amount.is_finite() || self.tts.min_amount < 0.0 || self.tts.min_amount > 1e9 {
            return Err(format!("{path}.tts.min_amount: must be a number from 0 up."));
        }
        check_css(&format!("{path}.css"), &self.css)
    }
}

impl ThemeContent {
    fn validate(&self) -> Check {
        if self.scenes.is_empty() || self.scenes.len() > MAX_SCENES {
            return Err(format!("content.scenes: a theme has 1 to {MAX_SCENES} scenes."));
        }
        let mut keys = std::collections::HashSet::new();
        for (i, s) in self.scenes.iter().enumerate() {
            if !keys.insert(s.key.as_str()) {
                return Err(format!("content.scenes[{i}].key: two scenes are both \"{}\".", s.key));
            }
            s.validate(&format!("content.scenes[{i}]"))?;
        }
        if let Some(a) = &self.alert_design
            && !is_slug(a, 80)
        {
            return Err("content.alert_design: that isn't a design id.".into());
        }
        Ok(())
    }
}

impl Scene {
    fn validate(&self, path: &str) -> Check {
        let custom = self
            .key
            .strip_prefix("custom_")
            .is_some_and(|n| !n.is_empty() && n.len() <= 3 && n.bytes().all(|b| b.is_ascii_digit()));
        if !SCENE_KEYS.contains(&self.key.as_str()) && !custom {
            return Err(format!("{path}.key: use one of {} (or custom_1 to custom_999).", SCENE_KEYS.join(", ")));
        }
        let n = self.name.trim().chars().count();
        if n == 0 || n > 40 {
            return Err(format!("{path}.name: give the scene a name of 1 to 40 characters."));
        }
        if let Some(bg) = &self.background {
            color(&format!("{path}.background"), bg)?;
        }
        let as_overlay = self.as_overlay("scene", None);
        as_overlay.validate().map_err(|e| format!("{path}.layers: {e}."))
    }

    /// The overlay this scene becomes (also how its layers are checked).
    pub fn as_overlay(&self, id: &str, design: Option<crate::overlay::DesignRef>) -> Overlay {
        Overlay {
            id: id.into(),
            name: self.name.clone(),
            token: String::new(),
            width: self.width,
            height: self.height,
            widgets: self.layers.clone(),
            updated_at: 0,
            design,
        }
    }
}

impl WidgetContent {
    fn validate(&self) -> Check {
        if self.widget == WidgetKind::Layer {
            return Err("content.widget: a widget design styles a live widget, not a layer.".into());
        }
        within("content.w", self.w, 40, 1920, " px")?;
        within("content.h", self.h, 20, 1080, " px")?;
        if !self.config.is_object() || self.config.get("style").is_some() {
            return Err("content.config: must be the widget's settings, without its style.".into());
        }
        if serde_json::to_string(&self.config).map(|s| s.len()).unwrap_or(usize::MAX) > 16 * 1024 {
            return Err("content.config: the settings are too big.".into());
        }
        layer::validate_widget_style(&self.style).map_err(|e| e.replacen("config.style", "content.style", 1))
    }

    /// The widget config with the style folded in, as an overlay stores it.
    pub fn config_with_style(&self, design_id: &str, branded: bool) -> serde_json::Value {
        let mut cfg = self.config.clone();
        if let Some(obj) = cfg.as_object_mut() {
            obj.insert("style".into(), self.style.clone());
            obj.insert("design_id".into(), serde_json::Value::String(design_id.into()));
            obj.insert("branded".into(), serde_json::Value::Bool(branded));
        }
        cfg
    }
}

/// A layer that draws the Spark mark of a built-in design.
pub fn is_brand_layer(l: &Widget) -> bool {
    l.kind == WidgetKind::Layer && l.config.get("type").and_then(serde_json::Value::as_str) == Some("brand")
}

// ------------------------------------------------------------------------------- render

/// What the overlay gets with an alert that uses a design: the look itself plus the text
/// already split into highlighted runs (the name, the amount).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertDesignView {
    pub design_id: String,
    pub variation_id: String,
    pub event_key: EventKey,
    pub branded: bool,
    pub look: AlertLook,
    pub title_parts: Vec<TextPart>,
    pub body_parts: Vec<TextPart>,
    /// How hard the alert lands when the look's `motion.by_amount` is on (`intensity_of`).
    #[serde(default = "one")]
    pub intensity: f32,
}

fn one() -> f32 {
    1.0
}

/// How hard a "Bigger tips hit harder" alert lands: 1 for a usual amount, down to 0.6 for a
/// tenth of it and up to 2 for a hundred times it. Money is compared with a usual tip in its
/// own currency, Bits, KICKs and Jewels with 500, a batch of gifts with 5. Other events
/// (follows, subs, raids) are 1.
pub fn intensity_of(event: &Event) -> f32 {
    let (amount, usual) = match &event.kind {
        EventKind::Tip { money, .. } | EventKind::PaidMessage { money, .. } => {
            (money.major(), usual_tip(&money.currency))
        }
        EventKind::PlatformGift { value, .. } => (value.amount as f64, 500.0),
        EventKind::GiftBatch { count, .. } => (f64::from(*count), 5.0),
        _ => return 1.0,
    };
    intensity_for(amount / usual)
}

/// `intensity_of` for an amount `ratio` times the usual one.
pub fn intensity_for(ratio: f64) -> f32 {
    if !ratio.is_finite() || ratio <= 0.0 {
        return 1.0;
    }
    let l = ratio.log10();
    let k = if l >= 0.0 { 1.0 + 0.5 * l } else { 1.0 + 0.4 * l };
    k.clamp(0.6, 2.0) as f32
}

/// A usual tip in major units: about five US dollars, rounded to what people send.
fn usual_tip(currency: &str) -> f64 {
    match currency.to_ascii_uppercase().as_str() {
        "INR" => 500.0,
        "JPY" => 700.0,
        "KRW" => 7000.0,
        "IDR" => 80_000.0,
        "VND" => 120_000.0,
        "PHP" => 250.0,
        "PKR" => 1400.0,
        "BDT" => 600.0,
        "THB" => 170.0,
        "TWD" => 160.0,
        "HUF" => 1800.0,
        "CZK" => 110.0,
        "TRY" => 170.0,
        "RUB" => 450.0,
        "MXN" => 90.0,
        "BRL" => 25.0,
        "ARS" => 5000.0,
        "CLP" => 4500.0,
        "COP" => 20_000.0,
        "ZAR" => 90.0,
        "NGN" => 7500.0,
        _ => 5.0,
    }
}

/// Variables shown in the highlight colour.
pub const HIGHLIGHT_VARS: [&str; 8] =
    ["name", "amount", "value", "count", "count_text", "viewers", "viewers_text", "reward"];

/// The look a design gives `event`: its own look (or the plainer one it falls back to), and
/// in it the highest-priority matching variation, else the default.
pub fn pick<'a>(design: &'a Design, event: &Event) -> Option<(EventKey, &'a DesignVariation)> {
    let DesignContent::Alert(a) = &design.content else { return None };
    let key = event_key(event)?;
    let looks = key.fallbacks().iter().find_map(|k| a.events.get(k))?;
    let mut best: Option<&DesignVariation> = None;
    for v in &looks.variations {
        if v.condition.as_ref().is_some_and(|c| c.matches(event)) && best.is_none_or(|b| v.priority >= b.priority) {
            best = Some(v);
        }
    }
    best.or_else(|| looks.variations.iter().find(|v| v.condition.is_none())).map(|v| (key, v))
}

/// Whether the look lets text-to-speech read this event's message (the TTS policy still
/// decides on top of this).
pub fn allows_tts(look: &AlertLook, event: &Event) -> bool {
    look.tts.enabled && paid_amount(event).is_none_or(|a| a >= look.tts.min_amount)
}

/// What the TTS minimum compares against: money (major units) or Bits/KICKs. Sub months,
/// raid viewers and gift counts aren't amounts someone paid, so they're never held back.
fn paid_amount(event: &Event) -> Option<f64> {
    match &event.kind {
        EventKind::Tip { .. } | EventKind::PaidMessage { .. } | EventKind::PlatformGift { .. } => {
            amount_of(event, None)
        }
        _ => None,
    }
}

/// Fills `alert` (already rendered from the rule) with the design's look: text from the
/// look's templates, its media, sound, length and CSS.
pub fn render_onto(alert: &mut RenderedAlert, design: &Design, key: EventKey, v: &DesignVariation, event: &Event) {
    let vars = event_vars(event);
    let parts = |src: &str| {
        Template::parse(&with_plurals(src))
            .map(|t| t.render_parts(&vars, &HIGHLIGHT_VARS))
            .unwrap_or_else(|_| vec![TextPart { text: src.to_string(), hl: false }])
    };
    let join = |p: &[TextPart]| p.iter().map(|t| t.text.as_str()).collect::<String>();
    let look = &v.look;
    let title_parts = parts(&look.title);
    let body_parts = parts(&look.body);
    alert.title = join(&title_parts);
    alert.body = join(&body_parts);
    let user_media = |r: &Option<String>| r.clone().filter(|r| !r.starts_with("asset:"));
    alert.image = match look.media.kind {
        MediaKind::Image | MediaKind::Video => user_media(&look.media.media_ref),
        MediaKind::None | MediaKind::Emblem => None,
    };
    alert.sound = user_media(&look.sound.media_ref);
    alert.volume = look.sound.volume.min(100);
    alert.duration_ms = look.duration_ms.clamp(MIN_DURATION_MS, MAX_DURATION_MS);
    alert.layout = "design".into();
    alert.css = look.css.clone();
    alert.design = Some(AlertDesignView {
        design_id: design.id.clone(),
        variation_id: v.id.clone(),
        event_key: key,
        branded: design.branded,
        look: look.clone(),
        title_parts,
        body_parts,
        intensity: intensity_of(event),
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::alert::{AlertRule, Variation, render, select};
    use crate::event::{Millis, Origin as EventOrigin};
    use crate::money::{Money, PlatformValue};
    use crate::sample::{any_sample, sample};
    use serde_json::json;

    #[test]
    fn every_built_in_sound_id_reads_and_writes_back() {
        for id in [
            "none",
            "sparkle",
            "glint",
            "cascade",
            "bloom",
            "lift",
            "tally",
            "halo",
            "droplet",
            "kalimba",
            "marimba",
            "glow",
            "ember",
            "pebble",
            "feather",
            "hearth",
            "wink",
            "sparkle_classic",
            // retired in 1.1, still valid in saved designs
            "chime",
            "coin",
            "whoosh",
            "boing",
            "bass",
        ] {
            let s: BuiltinSound = serde_json::from_value(serde_json::json!(id)).expect(id);
            assert_eq!(serde_json::to_value(s).unwrap(), serde_json::json!(id));
        }
        assert!(serde_json::from_value::<BuiltinSound>(serde_json::json!("airhorn")).is_err());
    }

    pub(crate) fn look(title: &str) -> AlertLook {
        AlertLook {
            frame: Frame::Plate,
            layout: Layout::Above,
            media: MediaSlot { kind: MediaKind::Emblem, media_ref: None },
            media_size: 150,
            offsets: Offsets::default(),
            title: title.into(),
            body: "{message}".into(),
            font: Font {
                family: "Bricolage Grotesque".into(),
                source: FontSource::Bundled,
                size: 42,
                weight: 800,
                uppercase: false,
                italic: false,
                letter_spacing: 0.0,
            },
            colors: Colors {
                text: "#eef0f4".into(),
                highlight: "#ffa424".into(),
                plate: "#101216".into(),
                accent: "#33d4ff".into(),
                outline: "#07080a".into(),
            },
            plate: true,
            shadow: Shadow::Soft,
            outline: 0.0,
            anim: Anim {
                enter: AnimIn::Magnet,
                exit: AnimOut::Sink,
                text: TextAnim::Wave,
                in_ms: 650,
                out_ms: 450,
                text_delay_ms: 300,
            },
            motion: Motion::default(),
            sound: Sound { media_ref: None, builtin: BuiltinSound::Chime, volume: 70 },
            duration_ms: 6000,
            tts: Tts { enabled: true, min_amount: 0.0 },
            brand: BrandSpot::Corner,
            css: String::new(),
        }
    }

    pub(crate) fn alert_design(id: &str) -> Design {
        let mut events = BTreeMap::new();
        for key in EVENT_KEYS {
            events.insert(
                key,
                EventLooks {
                    variations: vec![DesignVariation {
                        id: format!("{}-0", key.key()),
                        name: "Every one".into(),
                        condition: None,
                        priority: 0,
                        look: look("{name} did a thing"),
                    }],
                },
            );
        }
        Design {
            schema: SCHEMA,
            id: id.into(),
            name: "Molten plate".into(),
            description: String::new(),
            tags: Tags { style: StyleTag::Bold, motion: MotionTag::Smooth },
            pro: false,
            built_in: false,
            branded: false,
            origin: Origin { kind: OriginKind::Scratch, from: None },
            content: DesignContent::Alert(AlertContent { events }),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn live(kind: &str, p: Platform) -> Event {
        let mut e = sample(kind, p, Millis(0)).unwrap();
        e.origin = EventOrigin::Live;
        e
    }

    #[test]
    fn json_shape_is_kind_plus_content() {
        let d = alert_design("d1");
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["kind"], "alert");
        assert!(v["content"]["events"]["follow"]["variations"][0]["look"]["anim"]["in"] == "magnet");
        assert_eq!(v["origin"]["type"], "scratch");
        let back: Design = serde_json::from_value(v).unwrap();
        assert_eq!(back, d);
        d.validate().unwrap();
    }

    #[test]
    fn events_map_to_finer_keys_and_fall_back() {
        assert_eq!(event_key(&live("follow", Platform::Twitch)), Some(EventKey::Follow));
        let mut sub = live("subscription", Platform::Twitch);
        assert_eq!(event_key(&sub), Some(EventKey::Resub), "the sample is 6 months");
        if let EventKind::Subscription { months, .. } = &mut sub.kind {
            *months = 1;
        }
        assert_eq!(event_key(&sub), Some(EventKey::Sub));
        assert_eq!(event_key(&live("subscription", Platform::Youtube)), Some(EventKey::Member));
        assert_eq!(event_key(&live("platform_gift", Platform::Twitch)), Some(EventKey::Bits));
        let mut kicks = live("platform_gift", Platform::Twitch);
        if let EventKind::PlatformGift { value, .. } = &mut kicks.kind {
            *value = PlatformValue { unit: ValueUnit::Kicks, amount: 100 };
        }
        assert_eq!(event_key(&kicks), Some(EventKey::Kicks));
        assert_eq!(event_key(&any_sample("paid_message", Millis(0)).unwrap()), Some(EventKey::Superchat));
        assert_eq!(event_key(&any_sample("raid", Millis(0)).unwrap()), Some(EventKey::Raid));
        assert_eq!(event_key(&any_sample("reward_redemption", Millis(0)).unwrap()), Some(EventKey::Redeem));
        assert_eq!(event_key(&any_sample("gift_batch", Millis(0)).unwrap()), Some(EventKey::Gift));

        // A design with only a sub look still covers resubs and members.
        let mut d = alert_design("d");
        if let DesignContent::Alert(a) = &mut d.content {
            a.events.retain(|k, _| *k == EventKey::Sub || *k == EventKey::Tip);
        }
        assert_eq!(pick(&d, &live("subscription", Platform::Youtube)).unwrap().1.id, "sub-0");
        assert_eq!(pick(&d, &any_sample("paid_message", Millis(0)).unwrap()).unwrap().1.id, "tip-0");
        assert!(pick(&d, &live("follow", Platform::Twitch)).is_none(), "no look: the rule's own look plays");
    }

    #[test]
    fn variations_pick_like_rules_do() {
        let mut d = alert_design("d");
        if let DesignContent::Alert(a) = &mut d.content {
            let tip = a.events.get_mut(&EventKey::Tip).unwrap();
            let mut big = tip.variations[0].clone();
            big.id = "big".into();
            big.name = "Big tips".into();
            big.priority = 5;
            big.condition = Some(Condition::AmountAtLeast { value: 1000.0, currency: Some("INR".into()) });
            let mut huge = big.clone();
            huge.id = "huge".into();
            huge.priority = 9;
            huge.condition = Some(Condition::AmountAtLeast { value: 5000.0, currency: None });
            tip.variations.extend([big, huge]);
        }
        d.validate().unwrap();
        let tip_of = |minor: i64| {
            let mut e = any_sample("tip", Millis(0)).unwrap();
            if let EventKind::Tip { money, .. } = &mut e.kind {
                *money = Money::new(minor, "INR");
            }
            e
        };
        assert_eq!(pick(&d, &tip_of(50_000)).unwrap().1.id, "tip-0");
        assert_eq!(pick(&d, &tip_of(150_000)).unwrap().1.id, "big");
        assert_eq!(pick(&d, &tip_of(900_000)).unwrap().1.id, "huge");
    }

    #[test]
    fn rendering_fills_text_parts_media_and_keeps_viewer_text_plain() {
        let rules = crate::alert::default_rules();
        let mut e = any_sample("tip", Millis(0)).unwrap();
        if let EventKind::Tip { actor, message, .. } = &mut e.kind {
            actor.display_name = "<img src=x onerror=alert(1)>".into();
            *message = Some("{name} lol".into());
        }
        let (rule, var) = select(&rules, &e).unwrap();
        let mut a = render("a1", rule, var, &e);
        let mut d = alert_design("d9");
        d.branded = true;
        d.built_in = true;
        if let DesignContent::Alert(c) = &mut d.content {
            let l = &mut c.events.get_mut(&EventKey::Tip).unwrap().variations[0].look;
            l.title = "{name} tipped {amount}!".into();
            l.media = MediaSlot { kind: MediaKind::Image, media_ref: Some("asset:alerts/coin.png".into()) };
            l.sound = Sound { media_ref: Some("01hzsound".into()), builtin: BuiltinSound::Coin, volume: 55 };
            l.duration_ms = 999_999;
        }
        let (key, v) = pick(&d, &e).unwrap();
        render_onto(&mut a, &d, key, v, &e);
        let view = a.design.as_ref().unwrap();
        assert_eq!(a.title, "<img src=x onerror=alert(1)> tipped ₹500!");
        assert_eq!(view.title_parts[0], TextPart { text: "<img src=x onerror=alert(1)>".into(), hl: true });
        assert_eq!(view.title_parts[1], TextPart { text: " tipped ".into(), hl: false });
        assert_eq!(view.title_parts[2], TextPart { text: "₹500".into(), hl: true });
        assert_eq!(a.body, "{name} lol", "a viewer's message is never a template");
        assert_eq!(a.image, None, "bundled art travels in the look, not as a media id");
        assert_eq!(a.sound.as_deref(), Some("01hzsound"));
        assert_eq!(a.volume, 55);
        assert_eq!(a.duration_ms, MAX_DURATION_MS);
        assert_eq!(a.layout, "design");
        assert!(view.branded);
        assert_eq!(view.event_key, EventKey::Tip);
        // The view survives the alert queue's JSON round trip.
        let back: RenderedAlert = serde_json::from_value(serde_json::to_value(&a).unwrap()).unwrap();
        assert_eq!(back, a);
        // Old alerts in a saved queue (no `design` key) still load.
        let mut old = serde_json::to_value(&a).unwrap();
        old.as_object_mut().unwrap().remove("design");
        assert!(serde_json::from_value::<RenderedAlert>(old).unwrap().design.is_none());
    }

    #[test]
    fn tts_follows_the_look_and_its_minimum() {
        let mut l = look("x");
        let tip = any_sample("tip", Millis(0)).unwrap(); // ₹500
        assert!(allows_tts(&l, &tip));
        l.tts.min_amount = 501.0;
        assert!(!allows_tts(&l, &tip));
        l.tts.min_amount = 500.0;
        assert!(allows_tts(&l, &tip));
        let follow = any_sample("follow", Millis(0)).unwrap();
        assert!(allows_tts(&l, &follow), "no amount, no minimum");
        let mut raid = any_sample("raid", Millis(0)).unwrap();
        if let EventKind::Raid { viewers, .. } = &mut raid.kind {
            *viewers = 3;
        }
        assert!(allows_tts(&l, &raid), "raid viewers aren't a paid amount");
        l.tts.enabled = false;
        assert!(!allows_tts(&l, &tip));
    }

    #[test]
    fn duplicating_always_strips_branding() {
        let mut d = alert_design("builtin.molten");
        d.built_in = true;
        d.branded = true;
        d.pro = true;
        let mine = d.into_user("01abc".into(), Origin { kind: OriginKind::Duplicate, from: Some("Molten".into()) }, 7);
        assert!(!mine.built_in && !mine.branded && !mine.pro);
        assert_eq!(mine.created_at, 7);
        mine.validate().unwrap();
        let mut forged = alert_design("x");
        forged.branded = true;
        assert!(matches!(forged.validate(), Err(DesignError::Invalid(m)) if m.starts_with("branded:")));
    }

    fn err(d: &Design) -> String {
        match d.validate() {
            Err(DesignError::Invalid(m)) => m,
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    fn with_tip_look(f: impl FnOnce(&mut AlertLook)) -> Design {
        let mut d = alert_design("d");
        if let DesignContent::Alert(a) = &mut d.content {
            f(&mut a.events.get_mut(&EventKey::Tip).unwrap().variations[0].look);
        }
        d
    }

    #[test]
    fn every_look_field_is_checked_with_a_named_message() {
        type Edit = Box<dyn Fn(&mut AlertLook)>;
        let cases: Vec<(Edit, &str)> = vec![
            (Box::new(|l| l.media_size = 9_000), "look.media_size"),
            (Box::new(|l| l.media = MediaSlot { kind: MediaKind::Image, media_ref: None }), "look.media.ref"),
            (
                Box::new(|l| l.media = MediaSlot { kind: MediaKind::Video, media_ref: Some("https://x".into()) }),
                "look.media.ref",
            ),
            (Box::new(|l| l.offsets.text_x = 401), "look.offsets.text_x"),
            (Box::new(|l| l.title = "{name".into()), "look.title"),
            (Box::new(|l| l.title = "x".repeat(MAX_TITLE + 1)), "look.title"),
            (Box::new(|l| l.body = "x".repeat(MAX_BODY + 1)), "look.body"),
            (Box::new(|l| l.font.family = "Comic;}body{".into()), "look.font.family"),
            (Box::new(|l| l.font.size = 2), "look.font.size"),
            (Box::new(|l| l.font.weight = 1000), "look.font.weight"),
            (Box::new(|l| l.font.letter_spacing = f32::NAN), "look.font.letter_spacing"),
            (Box::new(|l| l.colors.highlight = "red".into()), "look.colors.highlight"),
            (Box::new(|l| l.outline = 13.0), "look.outline"),
            (Box::new(|l| l.anim.in_ms = 5000), "look.anim.in_ms"),
            (Box::new(|l| l.anim.text_delay_ms = 20_000), "look.anim.text_delay_ms"),
            (Box::new(|l| l.sound.media_ref = Some("../x".into())), "look.sound.ref"),
            (Box::new(|l| l.sound.volume = 101), "look.sound.volume"),
            (Box::new(|l| l.duration_ms = 500), "look.duration_ms"),
            (Box::new(|l| l.duration_ms = 1000), "look.duration_ms"),
            (Box::new(|l| l.tts.min_amount = -1.0), "look.tts.min_amount"),
            (Box::new(|l| l.css = "@import url(x);".into()), "look.css"),
            (Box::new(|l| l.motion.bounce = 101), "look.motion.bounce"),
            (Box::new(|l| l.motion.speed = 59), "look.motion.speed"),
            (Box::new(|l| l.motion.speed = 161), "look.motion.speed"),
            (Box::new(|l| l.motion.stagger = 91), "look.motion.stagger"),
            (Box::new(|l| l.motion.bpm = 69), "look.motion.bpm"),
            (Box::new(|l| l.motion.bpm = 171), "look.motion.bpm"),
            (Box::new(|l| l.motion.custom_color = "url(x)".into()), "look.motion.custom_color"),
        ];
        for (f, field) in cases {
            let m = err(&with_tip_look(f));
            assert!(m.contains(field), "{field}: {m}");
            assert!(m.starts_with("content.events.tip.variations[0]"), "{m}");
            assert!(m.ends_with('.'), "{m}");
        }
    }

    #[test]
    fn design_level_mistakes_are_refused() {
        let mut d = alert_design("d");
        d.name = "  ".into();
        assert!(err(&d).starts_with("name:"));
        let mut d = alert_design("d");
        d.id = "has spaces".into();
        assert!(err(&d).starts_with("id:"));
        let mut d = alert_design("d");
        d.description = "x".repeat(201);
        assert!(err(&d).starts_with("description:"));
        let mut d = alert_design("d");
        d.schema = 2;
        assert_eq!(d.validate(), Err(DesignError::TooNew(2)));
        let mut d = alert_design("d");
        if let DesignContent::Alert(a) = &mut d.content {
            a.events.clear();
        }
        assert!(err(&d).starts_with("content.events:"));
        let mut d = alert_design("d");
        if let DesignContent::Alert(a) = &mut d.content {
            let v = &mut a.events.get_mut(&EventKey::Raid).unwrap().variations;
            v[0].condition = Some(Condition::HasMessage);
        }
        assert!(err(&d).contains("exactly one default look"));
        let mut d = alert_design("d");
        if let DesignContent::Alert(a) = &mut d.content {
            let v = &mut a.events.get_mut(&EventKey::Raid).unwrap().variations;
            let copy = v[0].clone();
            v.push(DesignVariation { condition: Some(Condition::HasMessage), ..copy });
        }
        assert!(err(&d).contains("share the id"));
        let mut d = alert_design("d");
        if let DesignContent::Alert(a) = &mut d.content {
            let v = &mut a.events.get_mut(&EventKey::Raid).unwrap().variations;
            let mut c = v[0].clone();
            c.id = "deep".into();
            let mut cond = Condition::HasMessage;
            for _ in 0..6 {
                cond = Condition::All { of: vec![cond] };
            }
            c.condition = Some(cond);
            v.push(c);
        }
        assert!(err(&d).contains("nested at most"));
    }

    #[test]
    fn unknown_events_kinds_and_fields_of_the_wrong_type_never_parse() {
        let mut v = serde_json::to_value(alert_design("d")).unwrap();
        v["content"]["events"]["tiktok_gift"] = v["content"]["events"]["tip"].clone();
        assert!(serde_json::from_value::<Design>(v).is_err());
        let mut v = serde_json::to_value(alert_design("d")).unwrap();
        v["kind"] = json!("script");
        assert!(serde_json::from_value::<Design>(v).is_err());
        let mut v = serde_json::to_value(alert_design("d")).unwrap();
        v["content"]["events"]["tip"]["variations"][0]["look"]["frame"] = json!("iframe");
        assert!(serde_json::from_value::<Design>(v).is_err());
    }

    pub(crate) fn theme() -> Design {
        let scene = |key: &str, name: &str| Scene {
            key: key.into(),
            name: name.into(),
            width: 1920,
            height: 1080,
            background: Some("#0a0b0d".into()),
            layers: vec![
                Widget::new(
                    "bg",
                    WidgetKind::Layer,
                    (0, 0, 1920, 1080),
                    json!({ "type": "shape", "shape": "rect", "fill": "#101216" }),
                ),
                Widget::new(
                    "title",
                    WidgetKind::Layer,
                    (160, 420, 1600, 160),
                    json!({ "type": "text", "text": name, "size": 96 }),
                ),
                Widget::new("goal", WidgetKind::Goal, (660, 900, 600, 80), json!({ "goal_id": "", "theme": "molten" })),
                Widget::new(
                    "mark",
                    WidgetKind::Layer,
                    (1820, 1000, 64, 48),
                    json!({ "type": "brand", "variant": "mark" }),
                ),
            ],
        };
        Design {
            schema: SCHEMA,
            id: "builtin.forge".into(),
            name: "Forge".into(),
            description: "Molten edges".into(),
            tags: Tags { style: StyleTag::Bold, motion: MotionTag::Smooth },
            pro: false,
            built_in: true,
            branded: true,
            origin: Origin { kind: OriginKind::BuiltIn, from: None },
            content: DesignContent::Theme(ThemeContent {
                scenes: SCENE_KEYS.iter().map(|k| scene(k, "Starting soon")).collect(),
                alert_design: Some("builtin.molten".into()),
            }),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn a_copy_of_a_built_in_theme_loses_its_mark_layers() {
        let t = theme();
        assert!(t.has_brand_layer());
        let mine = t.clone().into_user("mine".into(), Origin { kind: OriginKind::Duplicate, from: None }, 0);
        assert!(!mine.has_brand_layer());
        mine.validate().unwrap();
        let mut sneaky = t;
        sneaky.built_in = false;
        sneaky.branded = false;
        assert!(err(&sneaky).contains("only in built-in designs"));
    }

    #[test]
    fn themes_and_scenes_check_their_layers_like_overlays() {
        let t = theme();
        t.validate().unwrap();
        assert_eq!(serde_json::to_value(&t).unwrap()["kind"], "theme");
        let mut bad = t.clone();
        if let DesignContent::Theme(c) = &mut bad.content {
            c.scenes[0].layers[1].x = 5000;
        }
        assert!(err(&bad).starts_with("content.scenes[0].layers:"));
        let mut bad = t.clone();
        if let DesignContent::Theme(c) = &mut bad.content {
            c.scenes[1].key = "in_game".into();
        }
        assert!(err(&bad).contains("two scenes"));
        let mut bad = t.clone();
        if let DesignContent::Theme(c) = &mut bad.content {
            c.scenes[0].key = "lobby".into();
        }
        assert!(err(&bad).starts_with("content.scenes[0].key"));
        let mut bad = t.clone();
        if let DesignContent::Theme(c) = &mut bad.content {
            c.scenes[0].layers[1].config = json!({ "type": "text", "color": "javascript:" });
        }
        assert!(err(&bad).contains("config.color"));
        let mut bad = t;
        if let DesignContent::Theme(c) = &mut bad.content {
            c.scenes.clear();
        }
        assert!(err(&bad).starts_with("content.scenes:"));
    }

    #[test]
    fn widget_designs_style_live_widgets_only() {
        let w = Design {
            content: DesignContent::Widget(WidgetContent {
                widget: WidgetKind::Goal,
                w: 640,
                h: 96,
                config: json!({ "goal_id": "", "theme": "molten" }),
                style: json!({ "variant": "segments", "accent": "#ffa424", "track": "#16191e", "radius": 12 }),
            }),
            ..alert_design("w")
        };
        w.validate().unwrap();
        let DesignContent::Widget(c) = &w.content else { unreachable!() };
        let cfg = c.config_with_style("w", false);
        assert_eq!(cfg["style"]["variant"], "segments");
        assert_eq!(cfg["design_id"], "w");
        let mut layer = w.clone();
        if let DesignContent::Widget(c) = &mut layer.content {
            c.widget = WidgetKind::Layer;
        }
        assert!(err(&layer).starts_with("content.widget"));
        let mut styled = w;
        if let DesignContent::Widget(c) = &mut styled.content {
            c.style = json!({ "accent": "nope" });
        }
        assert!(err(&styled).starts_with("content.style.accent"));
    }

    #[test]
    fn media_ids_and_remapping_skip_bundled_files() {
        let mut d = with_tip_look(|l| {
            l.media = MediaSlot { kind: MediaKind::Image, media_ref: Some("old1".into()) };
            l.sound.media_ref = Some("asset:sounds/coin.mp3".into());
        });
        assert_eq!(d.media_ids(), vec!["old1".to_string()]);
        d.remap_media(&BTreeMap::from([("old1".to_string(), "new1".to_string())]));
        assert_eq!(d.media_ids(), vec!["new1".to_string()]);
        let mut t = theme();
        if let DesignContent::Theme(c) = &mut t.content {
            c.scenes[0].layers.push(Widget::new(
                "pic",
                WidgetKind::Layer,
                (0, 0, 10, 10),
                json!({ "type": "image", "media": "pic1" }),
            ));
        }
        assert_eq!(t.media_ids(), vec!["pic1".to_string()]);
        t.remap_media(&BTreeMap::from([("pic1".to_string(), "pic2".to_string())]));
        assert_eq!(t.media_ids(), vec!["pic2".to_string()]);
    }

    #[test]
    fn a_rule_without_a_design_renders_exactly_as_before() {
        let e = any_sample("follow", Millis(0)).unwrap();
        let rule = AlertRule {
            id: "r".into(),
            name: "r".into(),
            enabled: true,
            kinds: vec!["follow".into()],
            platforms: vec![],
            variations: vec![Variation::new("v", "{name} followed!")],
        };
        let a = render("a", &rule, &rule.variations[0], &e);
        assert!(a.design.is_none());
        assert!(!serde_json::to_string(&a).unwrap().contains("\"design\""), "the wire shape is unchanged");
    }

    // ------------------------------------------------------------- motion (1.1)

    /// A look as 1.0 saved it: no `motion` key.
    fn look_1_0() -> serde_json::Value {
        let mut v = serde_json::to_value(look("{name} tipped")).unwrap();
        v.as_object_mut().unwrap().remove("motion");
        v
    }

    #[test]
    fn a_1_0_look_reads_with_motion_that_changes_nothing() {
        let v = look_1_0();
        assert!(v.get("motion").is_none());
        let l: AlertLook = serde_json::from_value(v).unwrap();
        assert_eq!(l.motion, Motion::default());
        assert_eq!(l.motion.idle, Idle::None, "no loop that 1.0 didn't have");
        assert_eq!(l.motion.position, Spot::Mc, "the card stays in the middle");
        assert!(!l.motion.impact_sounds, "impact hits are off by default");
        assert!(!l.motion.beat);
        assert_eq!(l.anim.enter, AnimIn::Magnet, "the entrance keeps its own id");
        // Saved again it carries the motion block, and reads back the same.
        let again = serde_json::to_value(&l).unwrap();
        assert_eq!(again["motion"]["position"], "mc");
        assert_eq!(serde_json::from_value::<AlertLook>(again).unwrap(), l);
        let mut d = alert_design("d");
        if let DesignContent::Alert(a) = &mut d.content {
            a.events.get_mut(&EventKey::Tip).unwrap().variations[0].look = l;
        }
        d.validate().unwrap();
    }

    #[test]
    fn the_shipped_fixture_from_1_0_still_reads_and_validates() {
        let d: Design = serde_json::from_str(include_str!("../fixtures/designs/alert-design.json")).unwrap();
        d.validate().unwrap();
        let DesignContent::Alert(a) = &d.content else { panic!("alert") };
        for looks in a.events.values() {
            for v in &looks.variations {
                assert_eq!(v.look.motion, Motion::default());
            }
        }
    }

    #[test]
    fn every_1_0_animation_id_maps_to_itself() {
        let ins = [
            "none",
            "fade",
            "rise",
            "drop",
            "slide_left",
            "slide_right",
            "zoom",
            "pop",
            "bounce",
            "flip",
            "wipe",
            "magnet",
        ];
        let outs = ["none", "fade", "sink", "lift", "slide_left", "slide_right", "zoom", "shrink", "flip", "wipe"];
        let texts = ["none", "wave", "wiggle", "bounce", "pop", "type", "glow"];
        for id in ins {
            let a: AnimIn = serde_json::from_value(json!(id)).unwrap();
            assert_eq!(serde_json::to_value(a).unwrap(), json!(id));
        }
        for id in outs {
            let a: AnimOut = serde_json::from_value(json!(id)).unwrap();
            assert_eq!(serde_json::to_value(a).unwrap(), json!(id));
        }
        for id in texts {
            let a: TextAnim = serde_json::from_value(json!(id)).unwrap();
            assert_eq!(serde_json::to_value(a).unwrap(), json!(id));
        }
        assert_eq!(ANIM_IN_NAMES, ins, "scene layers keep the simple entrances only");
    }

    #[test]
    fn the_motion_designers_ids_read_and_write() {
        let ins = [
            "slam",
            "elastic",
            "bolt",
            "glitch",
            "portal",
            "shatter",
            "pour",
            "flip_shine",
            "confetti_pop",
            "dash",
            "neon",
        ];
        let outs = ["dissolve", "black_hole", "zip", "fold", "shatter", "tv_off", "fall", "iris", "glitch"];
        let texts = ["cascade", "pop_wave", "decode", "shine", "neon", "rgb", "stretch", "spin"];
        for id in ins {
            let a: AnimIn = serde_json::from_value(json!(id)).unwrap();
            assert_eq!(serde_json::to_value(a).unwrap(), json!(id));
        }
        for id in outs {
            let a: AnimOut = serde_json::from_value(json!(id)).unwrap();
            assert_eq!(serde_json::to_value(a).unwrap(), json!(id));
        }
        for id in texts {
            let a: TextAnim = serde_json::from_value(json!(id)).unwrap();
            assert_eq!(serde_json::to_value(a).unwrap(), json!(id));
        }
        for bad in ["Slam", "slam ", "", "flip-shine"] {
            assert!(serde_json::from_value::<AnimIn>(json!(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_partial_motion_block_fills_the_rest_with_defaults() {
        let mut v = look_1_0();
        v["motion"] = json!({ "idle": "jelly", "position": "br", "recipe": "arcade" });
        let l: AlertLook = serde_json::from_value(v).unwrap();
        assert_eq!(l.motion.idle, Idle::Jelly);
        assert_eq!(l.motion.position, Spot::Br);
        assert_eq!(l.motion.recipe, Some(Recipe::Arcade));
        assert_eq!(l.motion.bpm, 120);
        assert_eq!(l.motion.speed, 100);
        // Wrong types and unknown names are refused, not half-read.
        for bad in [
            json!({ "idle": "spin" }),
            json!({ "position": "middle" }),
            json!({ "bounce": -1 }),
            json!({ "bpm": "fast" }),
            json!({ "speed": 1e9 }),
        ] {
            let mut v = look_1_0();
            v["motion"] = bad.clone();
            assert!(serde_json::from_value::<AlertLook>(v).is_err(), "{bad}");
        }
    }

    #[test]
    fn motion_edges_are_allowed() {
        let d = with_tip_look(|l| {
            l.motion = Motion {
                bounce: 100,
                speed: 160,
                stagger: 90,
                bpm: 170,
                custom_color: "#ABCDEF".into(),
                ..Motion::default()
            };
        });
        d.validate().unwrap();
        let d = with_tip_look(|l| {
            l.motion = Motion { bounce: 0, speed: 60, stagger: 0, bpm: 70, ..Motion::default() };
        });
        d.validate().unwrap();
    }

    #[test]
    fn intensity_follows_the_amount_on_a_log_scale() {
        let close = |a: f32, b: f32| (a - b).abs() < 1e-4;
        assert!(close(intensity_for(1.0), 1.0));
        assert!(close(intensity_for(10.0), 1.5));
        assert!(close(intensity_for(100.0), 2.0));
        assert!(close(intensity_for(10_000.0), 2.0), "capped");
        assert!(close(intensity_for(0.1), 0.6));
        assert!(close(intensity_for(0.0001), 0.6), "floored");
        for odd in [0.0, -3.0, f64::NAN, f64::INFINITY] {
            assert!(close(intensity_for(odd), 1.0), "{odd}");
        }
        assert!(close(intensity_of(&live("follow", Platform::Twitch)), 1.0));
    }

    #[test]
    fn tips_compare_with_a_usual_tip_in_their_own_currency() {
        let tip = |minor: i64, cur: &str| {
            let mut e = live("tip", Platform::Twitch);
            if let EventKind::Tip { money, .. } = &mut e.kind {
                *money = Money::new(minor, cur);
            }
            intensity_of(&e)
        };
        let close = |a: f32, b: f32| (a - b).abs() < 1e-3;
        assert!(close(tip(50_000, "INR"), 1.0), "₹500");
        assert!(close(tip(5_000_000, "INR"), 2.0), "₹50,000");
        assert!(close(tip(5_000, "INR"), 0.6), "₹50");
        assert!(close(tip(500, "USD"), 1.0), "$5");
        assert!(close(tip(500, "usd"), 1.0), "case doesn't matter");
        assert!(close(tip(700, "JPY"), 1.0), "¥700 (no minor unit)");
    }

    #[test]
    fn the_overlay_gets_the_intensity_and_old_views_read_as_one() {
        let d = alert_design("d");
        let mut e = live("tip", Platform::Twitch);
        if let EventKind::Tip { money, .. } = &mut e.kind {
            *money = Money::new(5_000_000, "INR");
        }
        let (key, v) = pick(&d, &e).unwrap();
        let rules = crate::alert::default_rules();
        let (rule, var) = select(&rules, &e).unwrap();
        let mut a = render("a1", rule, var, &e);
        render_onto(&mut a, &d, key, v, &e);
        let view = a.design.clone().unwrap();
        assert!((view.intensity - 2.0).abs() < 1e-3);
        let mut j = serde_json::to_value(&view).unwrap();
        j.as_object_mut().unwrap().remove("intensity");
        let back: AlertDesignView = serde_json::from_value(j).unwrap();
        assert!((back.intensity - 1.0).abs() < f32::EPSILON);
    }
}
