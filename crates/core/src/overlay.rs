//! Overlays: one OBS browser source holding positioned widgets (API.md §4 Overlays).
//!
//! The widget `config` is a JSON object owned by the overlay bundle; core only checks the
//! parts that keep the server safe (known kinds, sane geometry, bounded size).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::event::{Actor, Content, Event, EventKind, Millis, Platform};
use crate::layer::{Check, is_color, is_font_family};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WidgetKind {
    Alerts,
    Chat,
    Events,
    Goal,
    Label,
    Viewers,
    /// A chat poll's live results.
    Poll,
    /// A giveaway: how to enter, then the winner.
    Giveaway,
    /// Scrolling credits roll at stream end.
    EndCredits,
    /// Floating/bouncing chat emotes bursting across the screen.
    EmoteWall,
    /// One pinned chat message shown until dismissed or timed out.
    ChatHighlight,
    /// A static piece of the scene: image, text, shape, frame, webcam frame or the brand
    /// mark of a built-in design (`config.type`).
    Layer,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Widget {
    pub id: String,
    pub kind: WidgetKind,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    #[serde(default = "empty_object")]
    pub config: Value,
    /// The editor won't move or resize it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub locked: bool,
    /// Kept in the overlay but not shown on stream.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
    /// Degrees, clockwise, around the centre.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rotation: f32,
    /// 0 (invisible) to 1.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub opacity: f32,
}

impl Widget {
    /// A widget with no rotation, full opacity, unlocked and shown.
    pub fn new(id: &str, kind: WidgetKind, (x, y, w, h): (i32, i32, i32, i32), config: Value) -> Self {
        Widget { id: id.into(), kind, x, y, w, h, config, locked: false, hidden: false, rotation: 0.0, opacity: 1.0 }
    }
}

fn empty_object() -> Value {
    json!({})
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero(v: &f32) -> bool {
    *v == 0.0
}

fn one() -> f32 {
    1.0
}

fn is_one(v: &f32) -> bool {
    *v == 1.0
}

/// The design an overlay was made from ("Use" in the gallery), so a built-in keeps its mark.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesignRef {
    pub id: String,
    /// The scene key (`in_game`, `brb`…).
    pub scene: String,
    /// Set by the engine from the design, never by the client.
    #[serde(default)]
    pub branded: bool,
    /// The overlay changes when its design does (24 Oct pick 6A); set by the engine, and
    /// false once the overlay's layout is edited by hand.
    #[serde(default)]
    pub follows: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Overlay {
    pub id: String,
    pub name: String,
    /// The secret part of the overlay URL. Never logged; regenerating it kills old URLs.
    pub token: String,
    pub width: i32,
    pub height: i32,
    pub widgets: Vec<Widget>,
    pub updated_at: i64,
    /// The gallery design this overlay came from, if any.
    #[serde(default)]
    pub design: Option<DesignRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OverlayError {
    #[error("the name can't be empty or longer than 60 characters")]
    Name,
    #[error("the canvas must be between 320×180 and 7680×4320")]
    Size,
    #[error("an overlay can hold at most {0} widgets")]
    TooManyWidgets(usize),
    #[error("widget '{0}' is outside the canvas or has no size")]
    Geometry(String),
    #[error("widget '{0}': settings must be an object under 64 KB")]
    Config(String),
    #[error("two widgets share the id '{0}'")]
    DuplicateId(String),
    #[error("widget '{0}': rotation must be -360 to 360 degrees and opacity 0 to 1")]
    Transform(String),
    #[error("layer '{id}': {message}")]
    Layer { id: String, message: String },
    #[error("label '{id}': {message}")]
    Label { id: String, message: String },
    #[error("widget '{id}': {message}")]
    Style { id: String, message: String },
}

/// Supported stream label data sources (local only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LabelSource {
    LatestFollower,
    LatestSub,
    LatestGifter,
    LatestTip,
    LatestBits,
    LatestRaid,
    SessionFollows,
    SessionSubs,
    SessionBits,
    SessionTips,
    SessionRaids,
    TopTipSession,
    TopSupporterSession,
    MonthlyFollows,
    MonthlySubs,
    MonthlyBits,
    MonthlyTips,
    MonthlyRaids,
    TopTipMonthly,
    TopSupporterMonthly,
    AlltimeFollows,
    AlltimeSubs,
    AlltimeBits,
    AlltimeTips,
    AlltimeRaids,
    TopTipAlltime,
    TopSupporterAlltime,
    #[serde(alias = "combo_train")]
    HypeTrain,
}

impl LabelSource {
    pub fn as_str(self) -> &'static str {
        match self {
            LabelSource::LatestFollower => "latest_follower",
            LabelSource::LatestSub => "latest_sub",
            LabelSource::LatestGifter => "latest_gifter",
            LabelSource::LatestTip => "latest_tip",
            LabelSource::LatestBits => "latest_bits",
            LabelSource::LatestRaid => "latest_raid",
            LabelSource::SessionFollows => "session_follows",
            LabelSource::SessionSubs => "session_subs",
            LabelSource::SessionBits => "session_bits",
            LabelSource::SessionTips => "session_tips",
            LabelSource::SessionRaids => "session_raids",
            LabelSource::TopTipSession => "top_tip_session",
            LabelSource::TopSupporterSession => "top_supporter_session",
            LabelSource::MonthlyFollows => "monthly_follows",
            LabelSource::MonthlySubs => "monthly_subs",
            LabelSource::MonthlyBits => "monthly_bits",
            LabelSource::MonthlyTips => "monthly_tips",
            LabelSource::MonthlyRaids => "monthly_raids",
            LabelSource::TopTipMonthly => "top_tip_monthly",
            LabelSource::TopSupporterMonthly => "top_supporter_monthly",
            LabelSource::AlltimeFollows => "alltime_follows",
            LabelSource::AlltimeSubs => "alltime_subs",
            LabelSource::AlltimeBits => "alltime_bits",
            LabelSource::AlltimeTips => "alltime_tips",
            LabelSource::AlltimeRaids => "alltime_raids",
            LabelSource::TopTipAlltime => "top_tip_alltime",
            LabelSource::TopSupporterAlltime => "top_supporter_alltime",
            LabelSource::HypeTrain => "hype_train",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "latest_follower" => Some(Self::LatestFollower),
            "latest_sub" => Some(Self::LatestSub),
            "latest_gifter" => Some(Self::LatestGifter),
            "latest_tip" => Some(Self::LatestTip),
            "latest_bits" => Some(Self::LatestBits),
            "latest_raid" => Some(Self::LatestRaid),
            "session_follows" => Some(Self::SessionFollows),
            "session_subs" => Some(Self::SessionSubs),
            "session_bits" => Some(Self::SessionBits),
            "session_tips" => Some(Self::SessionTips),
            "session_raids" => Some(Self::SessionRaids),
            "top_tip_session" => Some(Self::TopTipSession),
            "top_supporter_session" => Some(Self::TopSupporterSession),
            "monthly_follows" => Some(Self::MonthlyFollows),
            "monthly_subs" => Some(Self::MonthlySubs),
            "monthly_bits" => Some(Self::MonthlyBits),
            "monthly_tips" => Some(Self::MonthlyTips),
            "monthly_raids" => Some(Self::MonthlyRaids),
            "top_tip_monthly" => Some(Self::TopTipMonthly),
            "top_supporter_monthly" => Some(Self::TopSupporterMonthly),
            "alltime_follows" => Some(Self::AlltimeFollows),
            "alltime_subs" => Some(Self::AlltimeSubs),
            "alltime_bits" => Some(Self::AlltimeBits),
            "alltime_tips" => Some(Self::AlltimeTips),
            "alltime_raids" => Some(Self::AlltimeRaids),
            "top_tip_alltime" => Some(Self::TopTipAlltime),
            "top_supporter_alltime" => Some(Self::TopSupporterAlltime),
            "hype_train" | "combo_train" => Some(Self::HypeTrain),
            _ => None,
        }
    }

    pub fn is_train(self) -> bool {
        matches!(self, Self::HypeTrain)
    }

    pub fn is_monthly(self) -> bool {
        matches!(
            self,
            Self::MonthlyFollows
                | Self::MonthlySubs
                | Self::MonthlyBits
                | Self::MonthlyTips
                | Self::MonthlyRaids
                | Self::TopTipMonthly
                | Self::TopSupporterMonthly
        )
    }
}

fn default_label_template() -> String {
    "{value}".into()
}
fn default_label_theme() -> String {
    "molten".into()
}
fn default_train_window_s() -> u32 {
    120
}
fn default_train_step_s() -> u32 {
    15
}
fn default_train_max_s() -> u32 {
    300
}
fn default_train_threshold() -> u32 {
    3
}

/// Typed options for a `WidgetKind::Label` widget config JSON object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelOptions {
    pub source: LabelSource,
    #[serde(default = "default_label_template")]
    pub template: String,
    #[serde(default = "default_label_theme")]
    pub theme: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default)]
    pub timezone_offset_min: i32,
    #[serde(default = "default_train_window_s")]
    pub train_window_s: u32,
    #[serde(default = "default_train_step_s")]
    pub train_step_s: u32,
    #[serde(default = "default_train_max_s")]
    pub train_max_s: u32,
    #[serde(default = "default_train_threshold")]
    pub train_threshold: u32,
}

/// Validates a `WidgetKind::Label` config object and parses its typed [`LabelOptions`].
pub fn validate_label(config: &Value) -> Result<LabelOptions, String> {
    let Some(obj) = config.as_object() else {
        return Err("config: must be an object.".into());
    };
    let Some(raw_src) = obj.get("source").and_then(Value::as_str) else {
        return Err("config.source: pick a label source.".into());
    };
    if LabelSource::parse(raw_src).is_none() {
        return Err(format!("config.source: unknown label source '{raw_src}'."));
    }
    if let Some(t) = obj.get("template") {
        let Some(s) = t.as_str() else {
            return Err("config.template: must be text.".into());
        };
        if s.chars().count() > 240 {
            return Err("config.template: can be up to 240 characters.".into());
        }
        crate::template::Template::parse(s).map_err(|e| format!("config.template: {e}."))?;
    }
    if let Some(theme) = obj.get("theme") {
        let Some(t) = theme.as_str() else {
            return Err("config.theme: must be text.".into());
        };
        if t.is_empty() || t.len() > 40 || !t.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')) {
            return Err("config.theme: use a valid theme id.".into());
        }
    }
    let opts: LabelOptions =
        serde_json::from_value(config.clone()).map_err(|e| format!("config: invalid label options ({e})."))?;
    if let Some(ref tz) = opts.timezone
        && !crate::labels::is_supported_timezone(tz)
    {
        return Err(format!("config.timezone: unsupported timezone '{tz}'."));
    }
    if !(-720..=840).contains(&opts.timezone_offset_min) {
        return Err("config.timezone_offset_min: must be -720 to 840 minutes.".into());
    }
    if !(10..=3600).contains(&opts.train_window_s) {
        return Err("config.train_window_s: must be 10 to 3600 seconds.".into());
    }
    if !(1..=600).contains(&opts.train_step_s) {
        return Err("config.train_step_s: must be 1 to 600 seconds.".into());
    }
    if !(10..=7200).contains(&opts.train_max_s) {
        return Err("config.train_max_s: must be 10 to 7200 seconds.".into());
    }
    if opts.train_max_s < opts.train_window_s {
        return Err("config.train_max_s: must be at least train_window_s.".into());
    }
    if !(1..=50).contains(&opts.train_threshold) {
        return Err("config.train_threshold: must be 1 to 50 events.".into());
    }
    Ok(opts)
}

pub const MAX_WIDGETS: usize = 40;
const MAX_CONFIG_BYTES: usize = 64 * 1024;

impl Overlay {
    pub fn validate(&self) -> Result<(), OverlayError> {
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > 60 {
            return Err(OverlayError::Name);
        }
        if !(320..=7680).contains(&self.width) || !(180..=4320).contains(&self.height) {
            return Err(OverlayError::Size);
        }
        if self.widgets.len() > MAX_WIDGETS {
            return Err(OverlayError::TooManyWidgets(MAX_WIDGETS));
        }
        let mut ids = std::collections::HashSet::new();
        for w in &self.widgets {
            if !ids.insert(w.id.as_str()) {
                return Err(OverlayError::DuplicateId(w.id.clone()));
            }
            let fits = w.w > 0
                && w.h > 0
                && w.x >= 0
                && w.y >= 0
                && w.x.checked_add(w.w).is_some_and(|r| r <= self.width)
                && w.y.checked_add(w.h).is_some_and(|b| b <= self.height);
            if !fits {
                return Err(OverlayError::Geometry(w.id.clone()));
            }
            let size = serde_json::to_string(&w.config).map(|s| s.len()).unwrap_or(usize::MAX);
            if !w.config.is_object() || size > MAX_CONFIG_BYTES {
                return Err(OverlayError::Config(w.id.clone()));
            }
            if !(w.rotation.is_finite() && (-360.0..=360.0).contains(&w.rotation))
                || !(w.opacity.is_finite() && (0.0..=1.0).contains(&w.opacity))
            {
                return Err(OverlayError::Transform(w.id.clone()));
            }
            if w.kind == WidgetKind::Layer {
                crate::layer::validate(&w.config)
                    .map_err(|message| OverlayError::Layer { id: w.id.clone(), message })?;
            } else {
                if w.kind == WidgetKind::Label {
                    validate_label(&w.config).map_err(|message| OverlayError::Label { id: w.id.clone(), message })?;
                }
                validate_widget_config(w.kind, &w.config)
                    .map_err(|message| OverlayError::Style { id: w.id.clone(), message })?;
                if let Some(style) = w.config.get("style") {
                    crate::layer::validate_widget_style(style)
                        .map_err(|message| OverlayError::Style { id: w.id.clone(), message })?;
                }
            }
        }
        if let Some(d) = &self.design
            && (d.id.is_empty() || d.id.len() > 80 || d.scene.is_empty() || d.scene.len() > 40)
        {
            return Err(OverlayError::Layer { id: "design".into(), message: "the design link is damaged".into() });
        }
        Ok(())
    }

    /// Whether the overlay still shows the brand mark a built-in design carries.
    pub fn has_brand_layer(&self) -> bool {
        self.widgets.iter().any(|w| {
            w.kind == WidgetKind::Layer && !w.hidden && w.config.get("type").and_then(Value::as_str) == Some("brand")
        })
    }

    /// Whether this overlay shows chat (so the server only streams chat where needed).
    pub fn wants_chat(&self) -> bool {
        self.widgets.iter().any(|w| w.kind == WidgetKind::Chat)
    }

    pub fn wants_alerts(&self) -> bool {
        self.widgets.iter().any(|w| w.kind == WidgetKind::Alerts)
    }

    pub fn wants_emotes(&self) -> bool {
        self.widgets.iter().any(|w| w.kind == WidgetKind::EmoteWall)
    }

    /// Live `chat_message` and moderation frames go to both Chat and Emote Wall widgets.
    pub fn wants_chat_events(&self) -> bool {
        self.wants_chat() || self.wants_emotes()
    }

    /// Whether a layer here moves or plays with the music (a motion background, the music
    /// piece or a sound element), so only these overlays get the many `music` frames.
    pub fn wants_music(&self) -> bool {
        self.widgets.iter().any(|w| {
            w.kind == WidgetKind::Layer
                && w.config.get("type").and_then(|t| t.as_str()).is_some_and(|t| matches!(t, "fx" | "music" | "sound"))
        })
    }
}

/// Starting layouts for "New overlay". `starter` is what first-run setup creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    Starter,
    AlertsOnly,
    ChatOnly,
    Blank,
}

/// The widgets for a preset on a 1920×1080 canvas. Ids are stable per preset so tests and
/// the editor can refer to them.
pub fn preset_widgets(preset: Preset) -> Vec<Widget> {
    let alerts =
        Widget::new("alerts", WidgetKind::Alerts, (560, 80, 800, 420), json!({ "theme": "molten", "rule_ids": null }));
    let chat = Widget::new(
        "chat",
        WidgetKind::Chat,
        (40, 560, 520, 480),
        json!({ "platforms": [], "max_messages": 30, "fade_after_s": null, "show_badges": true,
                "show_platform": true, "hide_commands": true, "hide_bots": true, "font_size": 20, "theme": "molten" }),
    );
    let events = Widget::new(
        "events",
        WidgetKind::Events,
        (1500, 40, 380, 320),
        json!({ "kinds": ["follow", "subscription", "gift_batch", "platform_gift", "paid_message", "tip", "raid"],
                "max_items": 6, "theme": "molten" }),
    );
    match preset {
        Preset::Starter => vec![alerts, chat, events],
        Preset::AlertsOnly => vec![alerts],
        Preset::ChatOnly => vec![Widget { x: 40, y: 40, w: 520, h: 1000, ..chat }],
        Preset::Blank => vec![],
    }
}

/// Hostnames Ferrocast already uses for Twitch, Kick, YouTube, 7TV, BTTV, and FFZ emotes.
pub const EMOTE_CDN_HOSTS: &[&str] = &[
    "static-cdn.jtvnw.net",
    "files.kick.com",
    "images.kick.com",
    "cdn.7tv.app",
    "cdn.betterttv.net",
    "cdn.frankerfacez.com",
    "yt3.ggpht.com",
];

pub const CREDITS_SECTIONS: &[&str] = &["followers", "subs", "gifters", "tippers", "raiders", "mods"];
pub const EMOTE_WALL_PHYSICS: &[&str] = &["bounce", "rain", "fountain"];
pub const WIDGET_THEMES: &[&str] = &["molten", "daylight", "arcade", "minimal", "sticker"];
pub const EMOTE_WALL_MAX_CAP: u64 = 120;

/// Accepts only `https://` URLs with no userinfo/port/backslash/whitespace/control characters and
/// an exact hostname match in [`EMOTE_CDN_HOSTS`].
pub fn is_trusted_emote_cdn(raw: &str) -> bool {
    if raw.len() > 512 || raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return false;
    }
    let Some(rest) = raw.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#', '\\']).next().unwrap_or("");
    if authority.is_empty()
        || authority.contains('@')
        || authority.contains(':')
        || authority.contains('\\')
        || !authority.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        || !rest[authority.len()..].starts_with('/')
        || rest.len() <= authority.len() + 1
    {
        return false;
    }
    let host = authority.to_ascii_lowercase();
    EMOTE_CDN_HOSTS.contains(&host.as_str())
}

/// A pinned chat message displayed in a `chat_highlight` overlay widget until dismissed or timed out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatHighlight {
    pub id: String,
    pub event_id: String,
    pub source_id: Option<String>,
    pub platform: Platform,
    pub actor: Actor,
    pub content: Content,
    pub is_action: bool,
    pub pinned_at: Millis,
    pub expires_at: Option<Millis>,
}

impl ChatHighlight {
    pub fn from_event(id: String, event: &Event, pinned_at: Millis, expires_at: Option<Millis>) -> Option<Self> {
        let EventKind::ChatMessage { actor, content, is_action, .. } = &event.kind else {
            return None;
        };
        Some(Self {
            id,
            event_id: event.id.clone(),
            source_id: event.source_id.clone(),
            platform: event.platform,
            actor: actor.clone(),
            content: content.clone(),
            is_action: *is_action,
            pinned_at,
            expires_at,
        })
    }
}

fn check_theme(obj: &serde_json::Map<String, Value>) -> Check {
    if let Some(theme) = obj.get("theme").filter(|v| !v.is_null()) {
        let s = theme.as_str().ok_or_else(|| "config.theme: must be text.".to_string())?;
        if !WIDGET_THEMES.contains(&s) {
            return Err("config.theme: choose molten, daylight, arcade, minimal or sticker.".into());
        }
    }
    Ok(())
}

fn check_optional_color(obj: &serde_json::Map<String, Value>, key: &str) -> Check {
    if let Some(v) = obj.get(key).filter(|v| !v.is_null()) {
        let s = v.as_str().ok_or_else(|| format!("config.{key}: must be a hex colour like #eef0f5."))?;
        if !s.is_empty() && !is_color(s) {
            return Err(format!("config.{key}: use a hex colour like #eef0f5 or clear it for the theme default."));
        }
    }
    Ok(())
}

fn check_optional_bool(obj: &serde_json::Map<String, Value>, key: &str) -> Check {
    if let Some(v) = obj.get(key)
        && !v.is_null()
        && !v.is_boolean()
    {
        return Err(format!("config.{key}: must be true or false."));
    }
    Ok(())
}

/// Validates widget `config` for interactive widget kinds (`end_credits`, `emote_wall`,
/// `chat_highlight`). Existing widget kinds keep backward-compatible validation here.
pub fn validate_widget_config(kind: WidgetKind, config: &Value) -> Check {
    if !matches!(kind, WidgetKind::EndCredits | WidgetKind::EmoteWall | WidgetKind::ChatHighlight) {
        return Ok(());
    }
    let obj = config.as_object().ok_or_else(|| "config: must be an object.".to_string())?;
    check_theme(obj)?;
    match kind {
        WidgetKind::EndCredits => {
            if let Some(title) = obj.get("title").filter(|v| !v.is_null()) {
                let t = title.as_str().ok_or_else(|| "config.title: must be text.".to_string())?;
                if t.chars().count() > 80 || t.chars().any(char::is_control) {
                    return Err("config.title: keep the heading on one line up to 80 characters.".into());
                }
            }
            if let Some(sections) = obj.get("sections") {
                let arr =
                    sections.as_array().ok_or_else(|| "config.sections: must be a list of sections.".to_string())?;
                if arr.is_empty() || arr.len() > CREDITS_SECTIONS.len() {
                    return Err("config.sections: choose 1 to 6 credits sections.".into());
                }
                let mut seen = Vec::with_capacity(arr.len());
                for item in arr {
                    let s = item.as_str().ok_or_else(|| "config.sections: each section must be text.".to_string())?;
                    if !CREDITS_SECTIONS.contains(&s) {
                        return Err(format!("config.sections: unknown section '{s}'."));
                    }
                    if seen.contains(&s) {
                        return Err(format!("config.sections: '{s}' is listed twice."));
                    }
                    seen.push(s);
                }
            }
            if let Some(speed) = obj.get("speed") {
                let n = speed
                    .as_f64()
                    .filter(|n| n.is_finite())
                    .ok_or_else(|| "config.speed: scroll speed must be a number from 20 to 300.".to_string())?;
                if !(20.0..=300.0).contains(&n) {
                    return Err("config.speed: scroll speed must be between 20 and 300.".into());
                }
            }
            if let Some(font) = obj.get("font").filter(|v| !v.is_null()) {
                let f = font.as_str().ok_or_else(|| "config.font: must be a font name.".to_string())?;
                if !is_font_family(f) {
                    return Err("config.font: choose one of the built-in fonts or \"system\".".into());
                }
            }
            check_optional_color(obj, "text_color")?;
            check_optional_color(obj, "accent_color")?;
            check_optional_color(obj, "background")?;
            check_optional_bool(obj, "loop")?;
        }
        WidgetKind::EmoteWall => {
            if let Some(platforms) = obj.get("platforms") {
                let arr =
                    platforms.as_array().ok_or_else(|| "config.platforms: must be a list of platforms.".to_string())?;
                if arr.len() > 3 {
                    return Err("config.platforms: choose from twitch, youtube and kick.".into());
                }
                let mut seen = Vec::with_capacity(arr.len());
                for item in arr {
                    let p = item.as_str().ok_or_else(|| "config.platforms: each platform must be text.".to_string())?;
                    if !matches!(p, "twitch" | "youtube" | "kick") {
                        return Err("config.platforms: choose from twitch, youtube and kick.".into());
                    }
                    if seen.contains(&p) {
                        return Err(format!("config.platforms: '{p}' is listed twice."));
                    }
                    seen.push(p);
                }
            }
            if let Some(max) = obj.get("max_emotes") {
                let n = max
                    .as_u64()
                    .ok_or_else(|| "config.max_emotes: must be a whole number from 5 to 120.".to_string())?;
                if !(5..=EMOTE_WALL_MAX_CAP).contains(&n) {
                    return Err("config.max_emotes: must be a whole number from 5 to 120.".into());
                }
            }
            if let Some(size) = obj.get("size") {
                let n = size
                    .as_u64()
                    .ok_or_else(|| "config.size: must be a whole number from 24 to 128 px.".to_string())?;
                if !(24..=128).contains(&n) {
                    return Err("config.size: must be between 24 and 128 px.".into());
                }
            }
            if let Some(lifetime) = obj.get("lifetime_s") {
                let n = lifetime
                    .as_f64()
                    .filter(|n| n.is_finite())
                    .ok_or_else(|| "config.lifetime_s: must be a number from 1 to 20 seconds.".to_string())?;
                if !(1.0..=20.0).contains(&n) {
                    return Err("config.lifetime_s: must be between 1 and 20 seconds.".into());
                }
            }
            if let Some(physics) = obj.get("physics") {
                let p =
                    physics.as_str().ok_or_else(|| "config.physics: must be bounce, rain or fountain.".to_string())?;
                if !EMOTE_WALL_PHYSICS.contains(&p) {
                    return Err("config.physics: choose bounce, rain or fountain.".into());
                }
            }
        }
        WidgetKind::ChatHighlight => {
            if let Some(dur) = obj.get("duration_s").filter(|v| !v.is_null()) {
                let n = dur.as_u64().ok_or_else(|| {
                    "config.duration_s: leave empty to stay until dismissed, or pick 5 to 3600 seconds.".to_string()
                })?;
                if !(5..=3600).contains(&n) {
                    return Err(
                        "config.duration_s: leave empty to stay until dismissed, or pick 5 to 3600 seconds.".into()
                    );
                }
            }
            if let Some(size) = obj.get("font_size") {
                let n = size
                    .as_u64()
                    .ok_or_else(|| "config.font_size: must be a whole number from 14 to 48 px.".to_string())?;
                if !(14..=48).contains(&n) {
                    return Err("config.font_size: must be between 14 and 48 px.".into());
                }
            }
            check_optional_bool(obj, "show_badges")?;
            check_optional_bool(obj, "show_platform")?;
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlay(widgets: Vec<Widget>) -> Overlay {
        Overlay {
            id: "o".into(),
            name: "Main".into(),
            token: "t".into(),
            width: 1920,
            height: 1080,
            widgets,
            updated_at: 0,
            design: None,
        }
    }

    #[test]
    fn presets_are_valid() {
        for p in [Preset::Starter, Preset::AlertsOnly, Preset::ChatOnly, Preset::Blank] {
            overlay(preset_widgets(p)).validate().unwrap();
        }
        assert!(overlay(preset_widgets(Preset::Starter)).wants_chat());
        assert!(!overlay(preset_widgets(Preset::AlertsOnly)).wants_chat());
    }

    #[test]
    fn rejects_bad_geometry_names_sizes_and_configs() {
        let mut o = overlay(preset_widgets(Preset::AlertsOnly));
        o.widgets[0].x = 1800;
        assert!(matches!(o.validate(), Err(OverlayError::Geometry(_))));
        o.widgets[0].x = i32::MAX;
        assert!(matches!(o.validate(), Err(OverlayError::Geometry(_))), "no overflow");
        let mut o = overlay(preset_widgets(Preset::AlertsOnly));
        o.widgets[0].w = 0;
        assert!(matches!(o.validate(), Err(OverlayError::Geometry(_))));
        let mut o = overlay(vec![]);
        o.name = "  ".into();
        assert_eq!(o.validate(), Err(OverlayError::Name));
        o.name = "x".repeat(61);
        assert_eq!(o.validate(), Err(OverlayError::Name));
        let mut o = overlay(vec![]);
        o.width = 100;
        assert_eq!(o.validate(), Err(OverlayError::Size));
        let mut o = overlay(preset_widgets(Preset::AlertsOnly));
        o.widgets[0].config = json!("not an object");
        assert!(matches!(o.validate(), Err(OverlayError::Config(_))));
        o.widgets[0].config = json!({ "big": "x".repeat(70_000) });
        assert!(matches!(o.validate(), Err(OverlayError::Config(_))));
        let mut o = overlay(preset_widgets(Preset::AlertsOnly));
        o.widgets.push(o.widgets[0].clone());
        assert!(matches!(o.validate(), Err(OverlayError::DuplicateId(_))));
        let many: Vec<Widget> =
            (0..41).map(|i| Widget::new(&i.to_string(), WidgetKind::Label, (0, 0, 10, 10), json!({}))).collect();
        assert!(matches!(overlay(many).validate(), Err(OverlayError::TooManyWidgets(_))));
    }

    #[test]
    fn validates_label_sources_templates_and_train_bounds() {
        let mut o = overlay(vec![Widget::new(
            "lbl",
            WidgetKind::Label,
            (10, 10, 420, 64),
            json!({
                "source": "hype_train",
                "template": "Hype {value}",
                "theme": "molten",
                "train_window_s": 120,
                "train_step_s": 15,
                "train_max_s": 300,
                "train_threshold": 3,
                "timezone": "America/New_York",
                "timezone_offset_min": 330
            }),
        )]);
        assert!(o.validate().is_ok());

        for bad in [
            json!({}),
            json!({ "source": "cloud_unknown" }),
            json!({ "source": "monthly_tips", "template": "{unclosed" }),
            json!({ "source": "hype_train", "train_window_s": 5 }),
            json!({ "source": "hype_train", "train_window_s": 120, "train_max_s": 60 }),
            json!({ "source": "hype_train", "train_threshold": 0 }),
            json!({ "source": "monthly_subs", "timezone_offset_min": 900 }),
            json!({ "source": "monthly_subs", "timezone": "Mars/Olympus" }),
        ] {
            o.widgets[0].config = bad;
            assert!(matches!(o.validate(), Err(OverlayError::Label { .. })), "should fail: {}", o.widgets[0].config);
        }
    }

    #[test]
    fn validates_new_interactive_widget_configs_and_caps() {
        let ok_credits = Widget::new(
            "ec",
            WidgetKind::EndCredits,
            (100, 100, 800, 700),
            json!({
                "theme": "molten",
                "title": "Thanks for watching",
                "sections": ["followers", "subs", "gifters", "tippers", "raiders", "mods"],
                "speed": 60,
                "font": "system",
                "text_color": "#eef0f5",
                "accent_color": "#ffb347",
                "background": null,
                "loop": true
            }),
        );
        overlay(vec![ok_credits.clone()]).validate().unwrap();

        let mut bad_credits = ok_credits.clone();
        bad_credits.config["speed"] = json!(500);
        assert!(matches!(overlay(vec![bad_credits]).validate(), Err(OverlayError::Style { .. })));

        let mut bad_color = ok_credits.clone();
        bad_color.config["text_color"] = json!("javascript:alert(1)");
        assert!(matches!(overlay(vec![bad_color]).validate(), Err(OverlayError::Style { .. })));

        let mut bad_font = ok_credits.clone();
        bad_font.config["font"] = json!("Comic Sans; background: url(x)");
        assert!(matches!(overlay(vec![bad_font]).validate(), Err(OverlayError::Style { .. })));

        let mut bad_sections = ok_credits;
        bad_sections.config["sections"] = json!([]);
        assert!(matches!(overlay(vec![bad_sections]).validate(), Err(OverlayError::Style { .. })));

        let ok_emotes = Widget::new(
            "ew",
            WidgetKind::EmoteWall,
            (0, 0, 1920, 1080),
            json!({ "theme": "molten", "platforms": ["twitch", "kick"], "max_emotes": 60, "size": 56, "lifetime_s": 6, "physics": "fountain" }),
        );
        overlay(vec![ok_emotes.clone()]).validate().unwrap();
        assert!(overlay(vec![ok_emotes.clone()]).wants_emotes());
        assert!(overlay(vec![ok_emotes.clone()]).wants_chat_events());
        assert!(!overlay(vec![ok_emotes.clone()]).wants_chat());

        let mut bad_cap = ok_emotes.clone();
        bad_cap.config["max_emotes"] = json!(999);
        assert!(matches!(overlay(vec![bad_cap]).validate(), Err(OverlayError::Style { .. })));

        let mut bad_physics = ok_emotes;
        bad_physics.config["physics"] = json!("explode");
        assert!(matches!(overlay(vec![bad_physics]).validate(), Err(OverlayError::Style { .. })));

        let ok_highlight = Widget::new(
            "ch",
            WidgetKind::ChatHighlight,
            (40, 800, 620, 220),
            json!({ "theme": "arcade", "duration_s": 30, "font_size": 24, "show_badges": true, "show_platform": true }),
        );
        overlay(vec![ok_highlight.clone()]).validate().unwrap();

        let mut bad_hl = ok_highlight;
        bad_hl.config["duration_s"] = json!(2);
        assert!(matches!(overlay(vec![bad_hl]).validate(), Err(OverlayError::Style { .. })));
    }

    #[test]
    fn emote_cdn_allowlist_blocks_hostile_urls() {
        for ok in [
            "https://static-cdn.jtvnw.net/emoticons/v2/25/static/dark/2.0",
            "https://files.kick.com/emotes/4148074/fullsize",
            "https://images.kick.com/emotes/1/fullsize",
            "https://cdn.7tv.app/emote/01F6N/2x.webp",
            "https://cdn.betterttv.net/emote/abc/2x",
            "https://cdn.frankerfacez.com/emote/123/2",
            "https://yt3.ggpht.com/abc=w48-h48-c-k-nd",
        ] {
            assert!(is_trusted_emote_cdn(ok), "expected trusted: {ok}");
        }
        for bad in [
            "http://static-cdn.jtvnw.net/emoticons/v2/25/static/dark/2.0",
            "https://static-cdn.jtvnw.net.evil.com/x.png",
            "https://evil.com/@static-cdn.jtvnw.net/x.png",
            "https://user@static-cdn.jtvnw.net/x.png",
            "https://static-cdn.jtvnw.net:8443/x.png",
            "https://evil.com\\@static-cdn.jtvnw.net/x.png",
            "https://evil.com/https://static-cdn.jtvnw.net/x.png",
            "data:image/svg+xml,<svg onload=alert(1)>",
            "javascript:alert(1)",
            "//cdn.7tv.app/emote/1/1x.webp",
            "https://static\u{ad}-cdn.jtvnw.net/x.png",
            "https://static-cdn.jtvnw.net",
            "https://static-cdn.jtvnw.net/",
            "https://yt3.ggpht.com?a=1",
            "https://static-cdn.jtvnw.net#x",
        ] {
            assert!(!is_trusted_emote_cdn(bad), "expected rejected: {bad}");
        }
    }
}
