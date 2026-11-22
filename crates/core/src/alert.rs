//! Alert rules: which events show an alert, and which variation of it.
//!
//! A streamer sets one rule per kind ("Subscriptions") with a default look and optional
//! variations ("10+ months gets the gold one"). Selection is a pure function of the rules
//! and the event so the editor can preview it and tests can pin it down.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::design::{Anim, AnimIn, AnimOut, TextAnim};
use crate::event::{Event, EventKind, Platform};
use crate::layer::{check_css, is_color};
use crate::template::{KNOWN_VARS, Template, TemplateError, Vars, event_vars, with_plurals};

/// One condition. Variations combine them with all/any.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Condition {
    /// Amount in major units (₹500 → 500, 1500 bits → 1500, 5 gifted subs → 5).
    /// With `currency`, only money in that currency matches.
    AmountAtLeast {
        value: f64,
        currency: Option<String>,
    },
    AmountBelow {
        value: f64,
        currency: Option<String>,
    },
    TierAtLeast {
        tier: u8,
    },
    MonthsAtLeast {
        months: u32,
    },
    PlatformIs {
        platform: Platform,
    },
    IsGifted,
    HasMessage,
    MessageContains {
        text: String,
    },
    All {
        of: Vec<Condition>,
    },
    Any {
        of: Vec<Condition>,
    },
}

impl Condition {
    pub fn matches(&self, event: &Event) -> bool {
        match self {
            Condition::AmountAtLeast { value, currency } => {
                amount_of(event, currency.as_deref()).is_some_and(|a| a >= *value)
            }
            Condition::AmountBelow { value, currency } => {
                amount_of(event, currency.as_deref()).is_some_and(|a| a < *value)
            }
            Condition::TierAtLeast { tier } => match &event.kind {
                EventKind::Subscription { tier: t, .. } | EventKind::GiftBatch { tier: t, .. } => t.rank() >= *tier,
                _ => false,
            },
            Condition::MonthsAtLeast { months } => {
                matches!(&event.kind, EventKind::Subscription { months: m, .. } if m >= months)
            }
            Condition::PlatformIs { platform } => event.platform == *platform,
            Condition::IsGifted => {
                matches!(&event.kind, EventKind::Subscription { gifted_by: Some(_), .. } | EventKind::GiftBatch { .. })
            }
            Condition::HasMessage => message_of(event).is_some_and(|m| !m.trim().is_empty()),
            Condition::MessageContains { text } => {
                let needle = text.to_lowercase();
                !needle.is_empty() && message_of(event).is_some_and(|m| m.to_lowercase().contains(&needle))
            }
            Condition::All { of } => of.iter().all(|c| c.matches(event)),
            Condition::Any { of } => of.iter().any(|c| c.matches(event)),
        }
    }
}

/// The number conditions compare against, in major units.
pub fn amount_of(event: &Event, currency: Option<&str>) -> Option<f64> {
    let money_ok = |c: &str| currency.is_none_or(|want| want.eq_ignore_ascii_case(c));
    match &event.kind {
        EventKind::Tip { money, .. } | EventKind::PaidMessage { money, .. } if money_ok(&money.currency) => {
            Some(money.major())
        }
        EventKind::PlatformGift { value, .. } if currency.is_none() => Some(value.amount as f64),
        EventKind::GiftBatch { count, .. } if currency.is_none() => Some(f64::from(*count)),
        EventKind::Raid { viewers, .. } if currency.is_none() => Some(f64::from(*viewers)),
        EventKind::Subscription { months, .. } if currency.is_none() => Some(f64::from(*months)),
        EventKind::RewardRedemption { cost, .. } if currency.is_none() => Some(*cost as f64),
        _ => None,
    }
}

fn message_of(event: &Event) -> Option<&str> {
    match &event.kind {
        EventKind::Subscription { message: Some(m), .. }
        | EventKind::PlatformGift { message: Some(m), .. }
        | EventKind::PaidMessage { content: Some(m), .. } => Some(&m.text),
        EventKind::Tip { message: Some(m), .. } => Some(m),
        EventKind::RewardRedemption { input: Some(i), .. } => Some(i),
        _ => None,
    }
}

/// Kind of a custom schema field exposed by a variation's custom HTML/CSS alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CustomFieldKind {
    Text,
    Number,
    Color,
    Toggle,
    Select,
}

/// One typed custom field in a variation's schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomField {
    pub key: String,
    pub label: String,
    #[serde(rename = "type")]
    pub kind: CustomFieldKind,
    #[serde(default)]
    pub default: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

/// Default alert entrance, exit and text effect for rules without explicit `anim`.
pub fn default_alert_anim() -> Anim {
    Anim { enter: AnimIn::Fade, exit: AnimOut::Fade, text: TextAnim::None, in_ms: 400, out_ms: 350, text_delay_ms: 150 }
}

/// How an alert looks and sounds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Variation {
    pub id: String,
    pub name: String,
    /// `None` = the rule's default look.
    pub condition: Option<Condition>,
    /// Higher wins when several variations match.
    pub priority: i32,
    /// Chance share among matching variations of the same top priority (0 = never picked
    /// while a peer has weight; all zero = equal chances). Default 1.
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// Headline, e.g. "{name} subscribed for {months} months!"
    pub title: String,
    /// Second line; usually "{message}".
    pub body: String,
    pub image: Option<String>,
    pub sound: Option<String>,
    /// 0–100.
    pub volume: u8,
    pub duration_ms: u32,
    /// Read the message aloud (still subject to the TTS policy and approval queue).
    pub tts: bool,
    /// Layout preset name the overlay understands ("stack", "side", "banner"…).
    pub layout: String,
    /// Extra CSS the streamer wrote. The overlay scopes it to the alert (R-39).
    #[serde(default)]
    pub css: String,
    /// Entrance, exit and text animation timing for this variation (R25).
    #[serde(default = "default_alert_anim")]
    pub anim: Anim,
    /// Render this variation using custom HTML/CSS inside a sandboxed overlay iframe (R25).
    #[serde(default)]
    pub custom_enabled: bool,
    /// Streamer-authored HTML template rendered only inside a sandboxed iframe (R25).
    #[serde(default)]
    pub html: String,
    /// Schema defining editable fields exposed to custom HTML/CSS (R25).
    #[serde(default)]
    pub fields: Vec<CustomField>,
    /// Current override values keyed by `CustomField.key` (R25).
    #[serde(default)]
    pub field_values: BTreeMap<String, Value>,
}

fn default_weight() -> u32 {
    1
}

/// Largest weight the editor accepts (keeps the sum far from overflow).
pub const MAX_WEIGHT: u32 = 1_000;
/// Maximum characters in custom HTML for one alert variation.
pub const MAX_CUSTOM_HTML: usize = 20_000;
/// Maximum UTF-8 bytes in custom HTML for one alert variation.
pub const MAX_CUSTOM_HTML_BYTES: usize = 80_000;
/// Maximum custom schema fields per variation.
pub const MAX_CUSTOM_FIELDS: usize = 32;
/// Most looks one rule may hold.
pub const MAX_VARIATIONS: usize = 20;
/// Longest viewer-supplied value sent to the custom frame (characters).
pub const MAX_CUSTOM_VAR_CHARS: usize = 2_000;
pub const MAX_IN_MS: u32 = 3_000;
pub const MAX_OUT_MS: u32 = 3_000;
pub const MAX_TEXT_DELAY_MS: u32 = 10_000;
pub const MAX_FIELD_KEY_LEN: usize = 40;
pub const MAX_FIELD_LABEL_LEN: usize = 80;
pub const MAX_FIELD_TEXT_LEN: usize = 500;
pub const MAX_SELECT_OPTIONS: usize = 20;

/// Explicit static CDN hosts allowed in custom alert CSP (images/fonts only; no connect-src).
pub const ALLOWED_ASSET_HOSTS: [&str; 6] = [
    "https://static-cdn.jtvnw.net",
    "https://yt3.ggpht.com",
    "https://files.kick.com",
    "https://cdn.ferrocast.com",
    "https://cdn.discordapp.com",
    "https://fonts.gstatic.com",
];

impl Variation {
    pub fn new(id: &str, title: &str) -> Self {
        Variation {
            id: id.into(),
            name: "Default".into(),
            condition: None,
            priority: 0,
            weight: 1,
            title: title.into(),
            body: "{message}".into(),
            image: None,
            sound: None,
            volume: 70,
            duration_ms: 7_000,
            tts: false,
            layout: "stack".into(),
            css: String::new(),
            anim: default_alert_anim(),
            custom_enabled: false,
            html: String::new(),
            fields: Vec::new(),
            field_values: BTreeMap::new(),
        }
    }
}

/// An alert rule for one or more event kinds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertRule {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// Event kind names (`EventKind::name`).
    pub kinds: Vec<String>,
    /// Empty = every platform.
    #[serde(default)]
    pub platforms: Vec<Platform>,
    pub variations: Vec<Variation>,
    /// A new alert from this rule ends the one playing instead of waiting its turn.
    #[serde(default)]
    pub interrupt: bool,
}

/// A small seedable random generator (SplitMix64), so picks are repeatable in tests.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn seeded(seed: u64) -> Self {
        Rng(seed)
    }

    /// Seeded from the clock and a counter (each call differs, even in the same tick).
    pub fn from_clock() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
        Rng(nanos ^ COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `0..n` (`n` > 0).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }
}

/// Shortest and longest an alert may stay on screen.
pub const MIN_DURATION_MS: u32 = 1_000;
pub const MAX_DURATION_MS: u32 = 60_000;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RuleError {
    #[error("the rule needs at least one event type")]
    NoKinds,
    #[error("'{0}' isn't an event type that can show an alert")]
    UnknownKind(String),
    #[error("a rule can have up to {MAX_VARIATIONS} looks")]
    TooManyVariations,
    #[error("the rule needs a default look (a variation without a condition)")]
    NoDefault,
    #[error("variation '{name}': {source}")]
    Template { name: String, source: TemplateError },
    #[error("variation '{name}': unknown token '{{{token}}}'")]
    UnknownToken { name: String, token: String },
    #[error("variation '{0}': volume must be 0 to 100")]
    Volume(String),
    #[error("variation '{0}': duration must be 1 to 60 seconds")]
    Duration(String),
    #[error("variation '{0}': weight must be 0 to 1000")]
    Weight(String),
    #[error("variation '{name}': {reason}")]
    Anim { name: String, reason: String },
    #[error("variation '{name}': custom HTML can be up to {MAX_CUSTOM_HTML} characters")]
    CustomHtmlTooLong { name: String },
    #[error("{0}")]
    CustomCss(String),
    #[error("variation '{name}': {reason}")]
    CustomSchema { name: String, reason: String },
    #[error("variation '{name}': custom HTML {reason}")]
    CustomHtml { name: String, reason: String },
}

/// Event kinds a rule may target.
pub const ALERT_KINDS: [&str; 8] =
    ["follow", "subscription", "gift_batch", "platform_gift", "paid_message", "tip", "raid", "reward_redemption"];

fn schema_err(name: &str, reason: impl Into<String>) -> RuleError {
    RuleError::CustomSchema { name: name.to_string(), reason: reason.into() }
}

fn anim_err(name: &str, reason: impl Into<String>) -> RuleError {
    RuleError::Anim { name: name.to_string(), reason: reason.into() }
}

fn is_valid_field_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_FIELD_KEY_LEN {
        return false;
    }
    if !bytes[0].is_ascii_lowercase() {
        return false;
    }
    if !bytes[1..].iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_') {
        return false;
    }
    !matches!(key, "__proto__" | "constructor" | "prototype") && !KNOWN_VARS.contains(&key)
}

fn validate_field_value(var_name: &str, f: &CustomField, val: &Value) -> Result<(), RuleError> {
    let key = &f.key;
    match f.kind {
        CustomFieldKind::Text => {
            let s = val.as_str().ok_or_else(|| schema_err(var_name, format!("field '{key}': expected text")))?;
            if s.chars().count() > MAX_FIELD_TEXT_LEN {
                return Err(schema_err(
                    var_name,
                    format!("field '{key}': text can be up to {MAX_FIELD_TEXT_LEN} characters"),
                ));
            }
        }
        CustomFieldKind::Number => {
            let n = val
                .as_f64()
                .filter(|x| x.is_finite())
                .ok_or_else(|| schema_err(var_name, format!("field '{key}': expected a finite number")))?;
            if let Some(min) = f.min
                && n < min
            {
                return Err(schema_err(var_name, format!("field '{key}': value must be at least {min}")));
            }
            if let Some(max) = f.max
                && n > max
            {
                return Err(schema_err(var_name, format!("field '{key}': value must be at most {max}")));
            }
        }
        CustomFieldKind::Color => {
            let s =
                val.as_str().ok_or_else(|| schema_err(var_name, format!("field '{key}': expected a colour string")))?;
            if !is_color(s) {
                return Err(schema_err(var_name, format!("field '{key}': use #rrggbb or transparent")));
            }
        }
        CustomFieldKind::Toggle => {
            if !val.is_boolean() {
                return Err(schema_err(var_name, format!("field '{key}': expected true or false")));
            }
        }
        CustomFieldKind::Select => {
            let s = val
                .as_str()
                .ok_or_else(|| schema_err(var_name, format!("field '{key}': expected one of the select options")))?;
            if !f.options.iter().any(|opt| opt == s) {
                return Err(schema_err(var_name, format!("field '{key}': '{s}' isn't one of its options")));
            }
        }
    }
    Ok(())
}

fn validate_custom_fields(
    var_name: &str,
    fields: &[CustomField],
    values: &BTreeMap<String, Value>,
) -> Result<HashSet<String>, RuleError> {
    if fields.len() > MAX_CUSTOM_FIELDS {
        return Err(schema_err(var_name, format!("can have at most {MAX_CUSTOM_FIELDS} custom fields")));
    }
    let mut keys = HashSet::with_capacity(fields.len());
    let mut by_key = HashMap::with_capacity(fields.len());
    for f in fields {
        if !is_valid_field_key(&f.key) {
            return Err(schema_err(
                var_name,
                format!(
                    "field key '{}' must start with a lowercase letter, use a-z/0-9/_ (up to {MAX_FIELD_KEY_LEN} chars), and not collide with built-in tokens",
                    f.key
                ),
            ));
        }
        if !keys.insert(f.key.clone()) {
            return Err(schema_err(var_name, format!("duplicate custom field key '{}'", f.key)));
        }
        let label_len = f.label.trim().chars().count();
        if label_len == 0 || label_len > MAX_FIELD_LABEL_LEN {
            return Err(schema_err(
                var_name,
                format!("field '{}': label must be 1 to {MAX_FIELD_LABEL_LEN} characters", f.key),
            ));
        }
        if f.kind == CustomFieldKind::Number {
            if f.min.is_some_and(|x| !x.is_finite()) || f.max.is_some_and(|x| !x.is_finite()) {
                return Err(schema_err(var_name, format!("field '{}': min and max must be finite numbers", f.key)));
            }
            if let (Some(min), Some(max)) = (f.min, f.max)
                && min > max
            {
                return Err(schema_err(var_name, format!("field '{}': min cannot exceed max", f.key)));
            }
        }
        if f.kind == CustomFieldKind::Select {
            if f.options.is_empty() || f.options.len() > MAX_SELECT_OPTIONS {
                return Err(schema_err(
                    var_name,
                    format!("field '{}': select fields need 1 to {MAX_SELECT_OPTIONS} options", f.key),
                ));
            }
            let mut opt_seen = HashSet::with_capacity(f.options.len());
            for opt in &f.options {
                let n = opt.trim().chars().count();
                if n == 0 || n > MAX_FIELD_LABEL_LEN || !opt_seen.insert(opt.as_str()) {
                    return Err(schema_err(
                        var_name,
                        format!(
                            "field '{}': select options must be unique and 1 to {MAX_FIELD_LABEL_LEN} chars",
                            f.key
                        ),
                    ));
                }
            }
        }
        validate_field_value(var_name, f, &f.default)?;
        by_key.insert(f.key.as_str(), f);
    }
    for (k, val) in values {
        let Some(field) = by_key.get(k.as_str()) else {
            return Err(schema_err(var_name, format!("unknown custom field value '{k}'")));
        };
        validate_field_value(var_name, field, val)?;
    }
    Ok(keys)
}

fn is_allowed_token(token: &str, custom_keys: &HashSet<String>) -> bool {
    KNOWN_VARS.contains(&token) || custom_keys.contains(token)
}

fn validate_template_tokens(var_name: &str, src: &str, custom_keys: &HashSet<String>) -> Result<(), RuleError> {
    let parsed = Template::parse(src).map_err(|source| RuleError::Template { name: var_name.to_string(), source })?;
    for tok in parsed.variables() {
        if !is_allowed_token(tok, custom_keys) {
            return Err(RuleError::UnknownToken { name: var_name.to_string(), token: tok.to_string() });
        }
    }
    Ok(())
}

/// Tags custom HTML may not use: anything that runs code, navigates, loads a page or a
/// sound, or whose text the browser parses differently (raw text).
const BANNED_TAGS: [&str; 33] = [
    "script",
    "meta",
    "base",
    "form",
    "iframe",
    "frame",
    "frameset",
    "object",
    "embed",
    "link",
    "applet",
    "noscript",
    "noembed",
    "noframes",
    "xmp",
    "plaintext",
    "textarea",
    "title",
    "template",
    "math",
    "foreignobject",
    "use",
    "animate",
    "set",
    "animatemotion",
    "animatetransform",
    "input",
    "button",
    "select",
    "audio",
    "video",
    "source",
    "track",
];

/// Attributes that take a URL: only `#anchors`, small `data:image/` pictures and the
/// allowed picture hosts are fine in them.
const URL_ATTRS: [&str; 11] =
    ["href", "src", "xlink:href", "poster", "data", "background", "srcset", "cite", "longdesc", "manifest", "icon"];

/// Attributes a viewer's words may be written into (never a URL, style or handler).
fn token_attr_ok(name: &str) -> bool {
    matches!(name, "alt" | "title" | "aria-label" | "aria-description" | "aria-valuetext") || name.starts_with("data-")
}

fn html_err(name: &str, reason: impl Into<String>) -> RuleError {
    RuleError::CustomHtml { name: name.to_string(), reason: reason.into() }
}

/// `{token}` names in `text` (a `{{` is a literal brace).
fn tokens_in(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '{' {
            if chars.get(i + 1) == Some(&'{') {
                i += 2;
                continue;
            }
            // No closing brace anywhere after this one: nothing later can close either.
            let Some(rel_end) = chars[i + 1..].iter().position(|&c| c == '}') else { break };
            {
                let inner: String = chars[i + 1..i + 1 + rel_end].iter().collect();
                // The overlay reads `{name}` exactly (no spaces), so this does too.
                let raw = inner.split('|').next().unwrap_or("").to_string();
                if !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b':') {
                    out.push(raw);
                }
                i += rel_end + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn url_attr_ok(value: &str) -> bool {
    let v = value.trim().to_ascii_lowercase();
    if v.chars().any(|c| c.is_control() || c.is_whitespace()) || v.contains('\\') {
        return false;
    }
    if v.starts_with('#') {
        return true;
    }
    // Entities (`&#106;avascript:`) only matter where a scheme could hide; a picture host's
    // signed links (`?ex=…&hm=…`) legitimately carry `&`.
    if v.contains('&') && !v.starts_with("https://") {
        return false;
    }
    if let Some(rest) = v.strip_prefix("data:image/") {
        return ["png", "jpeg", "gif", "webp", "avif"].iter().any(|t| rest.starts_with(&format!("{t};")));
    }
    if let Some(rest) = v.strip_prefix("https://") {
        let host = rest.split(['/', '?', '#']).next().unwrap_or("");
        return ALLOWED_ASSET_HOSTS.iter().any(|h| h.strip_prefix("https://") == Some(host));
    }
    false
}

/// Reads custom HTML the way a browser splits it into tags, text and `<style>` blocks, and
/// refuses what could run code, navigate, fetch or break out of a token's context. The
/// overlay's sandbox page enforces the same rules on the parsed document; this is the
/// early, explained refusal at save time.
fn check_custom_html(name: &str, html: &str, custom_keys: &HashSet<String>) -> Result<(), RuleError> {
    let chars: Vec<char> = html.chars().collect();
    let lower: Vec<char> = html.to_ascii_lowercase().chars().collect();
    let unknown = |tok: &str| RuleError::UnknownToken { name: name.to_string(), token: tok.to_string() };
    let mut i = 0;
    let mut text = String::new();
    let flush_text = |text: &mut String| -> Result<(), RuleError> {
        for tok in tokens_in(text) {
            if !is_allowed_token(&tok, custom_keys) {
                return Err(unknown(&tok));
            }
        }
        text.clear();
        Ok(())
    };
    while i < chars.len() {
        if chars[i] != '<' {
            text.push(chars[i]);
            i += 1;
            continue;
        }
        let rest: String = lower[i..chars.len().min(i + 9)].iter().collect();
        if rest.starts_with("<!--") {
            let Some(end) = (i + 4..chars.len().saturating_sub(2)).find(|&j| lower[j..j + 3] == ['-', '-', '>']) else {
                return Err(html_err(name, "has a comment that never closes"));
            };
            i = end + 3;
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            let Some(end) = (i..chars.len()).find(|&j| chars[j] == '>') else {
                return Err(html_err(name, "has markup that never closes"));
            };
            i = end + 1;
            continue;
        }
        let next = chars.get(i + 1).copied().unwrap_or(' ');
        if !(next.is_ascii_alphabetic() || (next == '/' && chars.get(i + 2).is_some_and(char::is_ascii_alphabetic))) {
            text.push('<');
            i += 1;
            continue;
        }
        flush_text(&mut text)?;
        let closing = next == '/';
        i += if closing { 2 } else { 1 };
        let start = i;
        while i < chars.len() && !(chars[i].is_whitespace() || chars[i] == '/' || chars[i] == '>') {
            i += 1;
        }
        let tag: String = lower[start..i].iter().collect();
        if tag.contains('{') {
            return Err(html_err(name, "can't put a {token} in a tag name"));
        }
        if BANNED_TAGS.contains(&tag.as_str()) {
            return Err(html_err(name, format!("can't use <{tag}>: custom alerts are HTML and CSS only")));
        }
        // Attributes.
        let mut attrs: Vec<(String, String)> = Vec::new();
        loop {
            while i < chars.len() && (chars[i].is_whitespace() || chars[i] == '/') {
                i += 1;
            }
            if i >= chars.len() {
                return Err(html_err(name, "has a tag that never closes"));
            }
            if chars[i] == '>' {
                i += 1;
                break;
            }
            let astart = i;
            while i < chars.len() && !(chars[i].is_whitespace() || matches!(chars[i], '=' | '>' | '/')) {
                i += 1;
            }
            let aname: String = lower[astart..i].iter().collect();
            if aname.contains('{') || aname.is_empty() {
                return Err(html_err(name, "can't put a {token} where an attribute name goes"));
            }
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            let mut value = String::new();
            if chars.get(i) == Some(&'=') {
                i += 1;
                while i < chars.len() && chars[i].is_whitespace() {
                    i += 1;
                }
                match chars.get(i) {
                    Some(&q) if q == '"' || q == '\'' => {
                        i += 1;
                        let vstart = i;
                        while i < chars.len() && chars[i] != q {
                            i += 1;
                        }
                        if i >= chars.len() {
                            return Err(html_err(name, "has an attribute that never closes"));
                        }
                        value = chars[vstart..i].iter().collect();
                        i += 1;
                    }
                    _ => {
                        let vstart = i;
                        while i < chars.len() && !chars[i].is_whitespace() && chars[i] != '>' {
                            i += 1;
                        }
                        value = chars[vstart..i].iter().collect();
                    }
                }
            }
            attrs.push((aname, value));
        }
        if closing {
            continue;
        }
        for (aname, value) in &attrs {
            if aname.starts_with("on")
                || matches!(aname.as_str(), "srcdoc" | "formaction" | "action" | "http-equiv" | "ping")
            {
                return Err(html_err(name, format!("can't use the \"{aname}\" attribute")));
            }
            if URL_ATTRS.contains(&aname.as_str()) {
                if !tokens_in(value).is_empty() {
                    return Err(html_err(name, format!("can't put a {{token}} in \"{aname}\"")));
                }
                if !url_attr_ok(value) {
                    return Err(html_err(
                        name,
                        format!("\"{aname}\" can only be a #anchor, a data:image/ picture or an allowed picture host"),
                    ));
                }
                continue;
            }
            if aname == "style" {
                if !tokens_in(value).is_empty() {
                    return Err(html_err(name, "can't put a {token} in a style attribute"));
                }
                check_css(&format!("variation '{name}' style attribute"), &format!("x{{{value}}}"))
                    .map_err(RuleError::CustomCss)?;
                continue;
            }
            for tok in tokens_in(value) {
                if !token_attr_ok(aname) {
                    return Err(html_err(
                        name,
                        format!(
                            "can only put a {{token}} in text or in alt, title, aria-label and data-* (not \"{aname}\")"
                        ),
                    ));
                }
                if !is_allowed_token(&tok, custom_keys) {
                    return Err(unknown(&tok));
                }
            }
        }
        if tag == "style" {
            let close = (i..chars.len()).find(|&j| lower[j..].starts_with(&['<', '/', 's', 't', 'y', 'l', 'e']));
            let end = close.unwrap_or(chars.len());
            let css: String = chars[i..end].iter().collect();
            check_css(&format!("variation '{name}' <style>"), &css).map_err(RuleError::CustomCss)?;
            i = end;
        }
    }
    flush_text(&mut text)
}

impl AlertRule {
    /// Checks everything the editor should refuse to save.
    pub fn validate(&self) -> Result<(), RuleError> {
        if self.kinds.is_empty() {
            return Err(RuleError::NoKinds);
        }
        if let Some(bad) = self.kinds.iter().find(|k| !ALERT_KINDS.contains(&k.as_str())) {
            return Err(RuleError::UnknownKind(bad.clone()));
        }
        if self.variations.len() > MAX_VARIATIONS {
            return Err(RuleError::TooManyVariations);
        }
        if !self.variations.iter().any(|v| v.condition.is_none()) {
            return Err(RuleError::NoDefault);
        }
        for v in &self.variations {
            if v.volume > 100 {
                return Err(RuleError::Volume(v.name.clone()));
            }
            if !(MIN_DURATION_MS..=MAX_DURATION_MS).contains(&v.duration_ms) {
                return Err(RuleError::Duration(v.name.clone()));
            }
            if v.weight > MAX_WEIGHT {
                return Err(RuleError::Weight(v.name.clone()));
            }
            if v.anim.in_ms > MAX_IN_MS {
                return Err(anim_err(&v.name, format!("entrance timing must be 0 to {MAX_IN_MS} ms")));
            }
            if v.anim.out_ms > MAX_OUT_MS {
                return Err(anim_err(&v.name, format!("exit timing must be 0 to {MAX_OUT_MS} ms")));
            }
            if v.anim.text_delay_ms > MAX_TEXT_DELAY_MS {
                return Err(anim_err(&v.name, format!("text delay must be 0 to {MAX_TEXT_DELAY_MS} ms")));
            }
            if v.anim.in_ms.saturating_add(v.anim.out_ms) >= v.duration_ms {
                return Err(anim_err(&v.name, "entrance plus exit must be shorter than total alert duration"));
            }
            if v.anim.text_delay_ms >= v.duration_ms {
                return Err(anim_err(&v.name, "text effect delay must start before the alert ends"));
            }
            if v.html.len() > MAX_CUSTOM_HTML_BYTES || v.html.chars().count() > MAX_CUSTOM_HTML {
                return Err(RuleError::CustomHtmlTooLong { name: v.name.clone() });
            }
            check_css(&format!("variation '{}'.css", v.name), &v.css).map_err(RuleError::CustomCss)?;
            let custom_keys = validate_custom_fields(&v.name, &v.fields, &v.field_values)?;
            for src in [&v.title, &v.body] {
                validate_template_tokens(&v.name, src, &custom_keys)?;
            }
            check_custom_html(&v.name, &v.html, &custom_keys)?;
        }
        Ok(())
    }

    fn applies_to(&self, event: &Event) -> bool {
        self.enabled
            && self.kinds.iter().any(|k| k == event.kind.name())
            && (self.platforms.is_empty() || self.platforms.contains(&event.platform))
    }

    /// The variation for `event`: among the matching conditional ones the highest priority,
    /// ties broken by weight with `rng`; else the default.
    pub fn pick_with(&self, event: &Event, rng: &mut Rng) -> Option<&Variation> {
        let matching: Vec<&Variation> =
            self.variations.iter().filter(|v| v.condition.as_ref().is_some_and(|c| c.matches(event))).collect();
        let top = matching.iter().map(|v| v.priority).max();
        let tied: Vec<&Variation> = matching.into_iter().filter(|v| Some(v.priority) == top).collect();
        pick_weighted(&tied, rng).or_else(|| self.variations.iter().find(|v| v.condition.is_none()))
    }

    /// [`pick_with`](Self::pick_with) with a clock-seeded generator.
    pub fn pick(&self, event: &Event) -> Option<&Variation> {
        self.pick_with(event, &mut Rng::from_clock())
    }
}

/// One of `tied` by weight. A zero weight is never chosen while another has weight; when
/// all are zero they are equally likely (an alert is never dropped over a weight of 0).
fn pick_weighted<'a>(tied: &[&'a Variation], rng: &mut Rng) -> Option<&'a Variation> {
    match tied {
        [] => None,
        [only] => Some(only),
        _ => {
            let total: u64 = tied.iter().map(|v| u64::from(v.weight)).sum();
            if total == 0 {
                return Some(tied[rng.below(tied.len() as u64) as usize]);
            }
            let mut roll = rng.below(total);
            for v in tied {
                let w = u64::from(v.weight);
                if roll < w {
                    return Some(v);
                }
                roll -= w;
            }
            tied.last().copied()
        }
    }
}

/// Picks the first enabled rule that applies, and its variation.
pub fn select<'a>(rules: &'a [AlertRule], event: &Event) -> Option<(&'a AlertRule, &'a Variation)> {
    select_with(rules, event, &mut Rng::from_clock())
}

/// [`select`] with the caller's generator (seeded in tests).
pub fn select_with<'a>(rules: &'a [AlertRule], event: &Event, rng: &mut Rng) -> Option<(&'a AlertRule, &'a Variation)> {
    if !event.may_alert() {
        return None;
    }
    rules.iter().filter(|r| r.applies_to(event)).find_map(|r| r.pick_with(event, rng).map(|v| (r, v)))
}

/// Custom alert payload rendered strictly inside a sandboxed overlay iframe (R25).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomAlertRender {
    pub html: String,
    pub css: String,
    pub fields: BTreeMap<String, Value>,
    pub vars: Vars,
}

/// What the overlay receives: text already rendered, media by id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenderedAlert {
    pub alert_id: String,
    pub event_id: String,
    pub rule_id: String,
    pub variation_id: String,
    pub kind: String,
    pub platform: Platform,
    pub title: String,
    pub body: String,
    pub image: Option<String>,
    pub sound: Option<String>,
    pub volume: u8,
    pub duration_ms: u32,
    pub layout: String,
    pub css: String,
    /// Text for TTS if the variation wants it; the TTS policy decides if it's read.
    pub tts_text: Option<String>,
    /// Media id of the synthesised speech, once the engine made it.
    #[serde(default)]
    pub tts_audio: Option<String>,
    pub test: bool,
    /// Animation and timing overrides from the alert variation (R25).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anim: Option<Anim>,
    /// Present when custom HTML/CSS mode is enabled; rendered only inside a sandboxed iframe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<CustomAlertRender>,
    /// The look from the alert design in use, when there is one (API.md §4 Designs). Absent
    /// otherwise, so overlays without design support see the same alert as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design: Option<crate::design::AlertDesignView>,
}

fn value_as_template_text(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

fn resolve_custom_fields(v: &Variation) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for f in &v.fields {
        let val = v.field_values.get(&f.key).unwrap_or(&f.default).clone();
        out.insert(f.key.clone(), val);
    }
    out
}

/// Renders the chosen variation for `event`. Templates were validated when saved; a
/// template that fails to parse here falls back to its raw text rather than dropping a
/// paid alert.
pub fn render(alert_id: &str, rule: &AlertRule, v: &Variation, event: &Event) -> RenderedAlert {
    let base_vars: Vars = event_vars(event);
    let resolved_fields = resolve_custom_fields(v);
    let mut template_vars = base_vars.clone();
    for (k, val) in &resolved_fields {
        template_vars.entry(k.clone()).or_insert_with(|| value_as_template_text(val));
    }
    let fill = |src: &str| {
        Template::parse(&with_plurals(src)).map(|t| t.render(&template_vars)).unwrap_or_else(|_| src.to_string())
    };
    let body = fill(&v.body);
    let custom = v.custom_enabled.then(|| CustomAlertRender {
        html: v.html.clone(),
        css: v.css.clone(),
        fields: resolved_fields,
        vars: base_vars
            .into_iter()
            .map(|(k, v)| (k, v.chars().take(MAX_CUSTOM_VAR_CHARS).collect::<String>()))
            .collect(),
    });
    RenderedAlert {
        alert_id: alert_id.into(),
        event_id: event.id.clone(),
        rule_id: rule.id.clone(),
        variation_id: v.id.clone(),
        kind: event.kind.name().into(),
        platform: event.platform,
        title: fill(&v.title),
        tts_text: (v.tts && !body.trim().is_empty()).then(|| body.clone()),
        tts_audio: None,
        body,
        image: v.image.clone(),
        sound: v.sound.clone(),
        volume: v.volume.min(100),
        duration_ms: v.duration_ms.clamp(MIN_DURATION_MS, MAX_DURATION_MS),
        layout: v.layout.clone(),
        css: v.css.clone(),
        test: event.origin == crate::event::Origin::Test,
        anim: Some(v.anim.clone()),
        custom,
        design: None,
    }
}

/// A variation's `sound` that names a built-in sound instead of a library file
/// (`builtin:glint`); the overlay plays its own file. Passed through as is.
pub const BUILTIN_SOUND_PREFIX: &str = "builtin:";

/// The built-in sound each alert kind starts with on a new install (the owner's pre-matched
/// set, 20 Oct 2026). Saved rules never change.
pub fn default_sound(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "follow" => "glint",
        "subscription" => "cascade",
        "gift_batch" => "bloom",
        "platform_gift" | "paid_message" | "tip" => "tally",
        "raid" => "lift",
        "reward_redemption" => "pebble",
        _ => return None,
    })
}

/// The rules a new install starts with, so alerts work the moment an account is connected.
/// Each starts with its kind's built-in sound.
pub fn default_rules() -> Vec<AlertRule> {
    let rule = |id: &str, name: &str, kind: &str, title: &str| {
        let mut v = Variation::new(&format!("{id}-default"), title);
        v.sound = default_sound(kind).map(|s| format!("{BUILTIN_SOUND_PREFIX}{s}"));
        AlertRule {
            id: id.into(),
            name: name.into(),
            enabled: true,
            kinds: vec![kind.into()],
            platforms: vec![],
            variations: vec![v],
            interrupt: false,
        }
    };
    vec![
        rule("follow", "Follows", "follow", "{name} followed!"),
        rule("sub", "Subscriptions", "subscription", "{name} subscribed for {months_text}!"),
        rule("gift", "Gifted subs", "gift_batch", "{name} gifted {count_text}!"),
        rule("bits", "Bits and KICKs", "platform_gift", "{name} sent {value}!"),
        rule("superchat", "Super Chats", "paid_message", "{name} sent {amount}!"),
        rule("tip", "Tips", "tip", "{name} tipped {amount}!"),
        rule("raid", "Raids", "raid", "{name} is raiding with {viewers_text}!"),
        rule("redeem", "Channel points", "reward_redemption", "{name} redeemed {reward}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Millis, Origin};
    use crate::sample::{any_sample, sample};

    fn sub_rule() -> AlertRule {
        let mut gold = Variation::new("gold", "{name} is a legend: {months} months!");
        gold.name = "Gold".into();
        gold.priority = 10;
        gold.condition = Some(Condition::MonthsAtLeast { months: 12 });
        let mut silver = Variation::new("silver", "{name}: {months} months");
        silver.name = "Silver".into();
        silver.priority = 5;
        silver.condition = Some(Condition::MonthsAtLeast { months: 3 });
        AlertRule {
            id: "sub".into(),
            name: "Subs".into(),
            enabled: true,
            kinds: vec!["subscription".into()],
            platforms: vec![],
            variations: vec![Variation::new("default", "{name} subscribed"), silver, gold],
            interrupt: false,
        }
    }

    fn sub_with_months(m: u32) -> Event {
        let mut e = sample("subscription", Platform::Twitch, Millis(0)).unwrap();
        e.origin = Origin::Live;
        if let EventKind::Subscription { months, .. } = &mut e.kind {
            *months = m;
        }
        e
    }

    #[test]
    fn highest_priority_matching_variation_wins() {
        let rules = vec![sub_rule()];
        assert_eq!(select(&rules, &sub_with_months(1)).unwrap().1.id, "default");
        assert_eq!(select(&rules, &sub_with_months(6)).unwrap().1.id, "silver");
        assert_eq!(select(&rules, &sub_with_months(24)).unwrap().1.id, "gold");
    }

    #[test]
    fn disabled_rules_wrong_platform_and_backfill_are_skipped() {
        let mut rules = vec![sub_rule()];
        rules[0].platforms = vec![Platform::Kick];
        assert!(select(&rules, &sub_with_months(1)).is_none());
        rules[0].platforms.clear();
        rules[0].enabled = false;
        assert!(select(&rules, &sub_with_months(1)).is_none());
        rules[0].enabled = true;
        let mut e = sub_with_months(1);
        e.origin = Origin::Backfill;
        assert!(select(&rules, &e).is_none());
    }

    #[test]
    fn currency_conditions_never_cross_currencies() {
        let tip = any_sample("tip", Millis(0)).unwrap(); // ₹500
        assert!(Condition::AmountAtLeast { value: 500.0, currency: Some("INR".into()) }.matches(&tip));
        assert!(!Condition::AmountAtLeast { value: 5.0, currency: Some("USD".into()) }.matches(&tip));
        assert!(Condition::AmountBelow { value: 501.0, currency: None }.matches(&tip));
        let bits = sample("platform_gift", Platform::Twitch, Millis(0)).unwrap();
        assert!(!Condition::AmountAtLeast { value: 1.0, currency: Some("USD".into()) }.matches(&bits));
        assert!(Condition::AmountAtLeast { value: 500.0, currency: None }.matches(&bits));
    }

    #[test]
    fn empty_needle_never_matches() {
        let tip = any_sample("tip", Millis(0)).unwrap();
        assert!(!Condition::MessageContains { text: String::new() }.matches(&tip));
        assert!(Condition::MessageContains { text: "KEEP".into() }.matches(&tip));
    }

    #[test]
    fn validation_catches_editor_mistakes() {
        let mut r = sub_rule();
        assert!(r.validate().is_ok());
        r.variations.retain(|v| v.condition.is_some());
        assert_eq!(r.validate(), Err(RuleError::NoDefault));
        let mut r = sub_rule();
        r.kinds = vec!["chat_message".into()];
        assert!(matches!(r.validate(), Err(RuleError::UnknownKind(_))));
        let mut r = sub_rule();
        r.variations[0].title = "{name".into();
        assert!(matches!(r.validate(), Err(RuleError::Template { .. })));
        let mut r = sub_rule();
        r.variations[0].duration_ms = 0;
        assert!(matches!(r.validate(), Err(RuleError::Duration(_))));
        let mut r = sub_rule();
        r.variations[0].volume = 101;
        assert!(matches!(r.validate(), Err(RuleError::Volume(_))));
        let mut r = sub_rule();
        r.kinds.clear();
        assert_eq!(r.validate(), Err(RuleError::NoKinds));
    }

    #[test]
    fn default_rules_start_with_their_kinds_sound() {
        let rules = default_rules();
        let sound = |id: &str| {
            rules.iter().find(|r| r.id == id).and_then(|r| r.variations[0].sound.clone()).unwrap_or_default()
        };
        assert_eq!(sound("follow"), "builtin:glint");
        assert_eq!(sound("sub"), "builtin:cascade");
        assert_eq!(sound("gift"), "builtin:bloom");
        assert_eq!(sound("tip"), "builtin:tally");
        assert_eq!(sound("raid"), "builtin:lift");
        assert_eq!(sound("redeem"), "builtin:pebble");
        assert!(rules.iter().all(|r| r.variations.iter().all(|v| v.sound.is_some())));
        assert_eq!(default_sound("chat_message"), None);
        // A rendered alert carries the sound for the overlay to play.
        let r = rules.iter().find(|r| r.id == "follow").unwrap();
        let a = render("a1", r, &r.variations[0], &sample("follow", Platform::Twitch, Millis(0)).unwrap());
        assert_eq!(a.sound.as_deref(), Some("builtin:glint"));
    }

    #[test]
    fn default_rules_are_valid_and_cover_every_sample() {
        let rules = default_rules();
        for r in &rules {
            r.validate().unwrap();
        }
        for kind in crate::sample::TESTABLE {
            let e = any_sample(kind, Millis(0)).unwrap();
            let (rule, v) = select(&rules, &e).unwrap_or_else(|| panic!("no default rule for {kind}"));
            let a = render("a1", rule, v, &e);
            assert!(!a.title.contains('{'), "unrendered title for {kind}: {}", a.title);
            assert!(a.test);
        }
    }

    #[test]
    fn render_clamps_and_reads_tts_only_with_text() {
        let e = any_sample("tip", Millis(0)).unwrap();
        let mut v = Variation::new("v", "{name}");
        v.duration_ms = 999_999;
        v.volume = 250;
        v.tts = true;
        let rule = AlertRule {
            id: "r".into(),
            name: "r".into(),
            enabled: true,
            kinds: vec!["tip".into()],
            platforms: vec![],
            variations: vec![v.clone()],
            interrupt: false,
        };
        let a = render("a", &rule, &v, &e);
        assert_eq!(a.duration_ms, MAX_DURATION_MS);
        assert_eq!(a.volume, 100);
        assert_eq!(a.tts_text.as_deref(), Some("Keep it up!"));
        let follow = any_sample("follow", Millis(0)).unwrap();
        assert_eq!(render("a", &rule, &v, &follow).tts_text, None);
    }

    #[test]
    fn a_first_sub_reads_one_month_with_old_and_new_templates() {
        let defaults = default_rules();
        let sub_rule = defaults.iter().find(|r| r.id == "sub").unwrap();
        let one = sub_with_months(1);
        let a = render("a", sub_rule, &sub_rule.variations[0], &one);
        assert!(a.title.ends_with("subscribed for 1 month!"), "{}", a.title);
        let many = render("a", sub_rule, &sub_rule.variations[0], &sub_with_months(12));
        assert!(many.title.ends_with("subscribed for 12 months!"), "{}", many.title);
        // A rule saved with the old default text still reads right.
        let old = Variation::new("old", "{name} subscribed for {months} months!");
        assert!(render("a", sub_rule, &old, &one).title.ends_with("subscribed for 1 month!"));
        // And other wording is left alone.
        let own = Variation::new("own", "{months}");
        assert_eq!(render("a", sub_rule, &own, &one).title, "1");
    }

    fn weighted_rule(weights: &[u32]) -> AlertRule {
        let mut r = sub_rule();
        r.variations = vec![Variation::new("default", "{name} subscribed")];
        for (i, w) in weights.iter().enumerate() {
            let mut v = Variation::new(&format!("v{i}"), "{name}");
            v.condition = Some(Condition::MonthsAtLeast { months: 1 });
            v.priority = 5;
            v.weight = *w;
            r.variations.push(v);
        }
        r
    }

    fn counts(rule: &AlertRule, seed: u64, n: usize) -> std::collections::HashMap<String, usize> {
        let mut rng = Rng::seeded(seed);
        let e = sub_with_months(2);
        let mut m = std::collections::HashMap::new();
        for _ in 0..n {
            *m.entry(rule.pick_with(&e, &mut rng).unwrap().id.clone()).or_insert(0) += 1;
        }
        m
    }

    #[test]
    fn weights_shape_the_distribution() {
        let c = counts(&weighted_rule(&[1, 3]), 7, 20_000);
        let share = c["v1"] as f64 / 20_000.0;
        assert!((share - 0.75).abs() < 0.02, "share {share}");
    }

    #[test]
    fn the_same_seed_gives_the_same_picks() {
        let r = weighted_rule(&[1, 1, 1]);
        assert_eq!(counts(&r, 42, 500), counts(&r, 42, 500));
        let e = sub_with_months(2);
        let run = |seed| {
            let mut g = Rng::seeded(seed);
            (0..30).map(|_| r.pick_with(&e, &mut g).unwrap().id.clone()).collect::<Vec<_>>()
        };
        assert_eq!(run(5), run(5));
        assert_ne!(run(5), run(6));
    }

    #[test]
    fn a_zero_weight_is_never_picked_beside_a_weighted_peer() {
        let c = counts(&weighted_rule(&[0, 2, 0]), 1, 2_000);
        assert_eq!(c.len(), 1);
        assert_eq!(c["v1"], 2_000);
    }

    #[test]
    fn all_zero_weights_are_equally_likely_and_still_alert() {
        let c = counts(&weighted_rule(&[0, 0]), 3, 4_000);
        assert_eq!(c.len(), 2);
        assert!(c.values().all(|n| (*n as f64 / 4_000.0 - 0.5).abs() < 0.05), "{c:?}");
    }

    #[test]
    fn one_variation_is_always_picked_whatever_its_weight() {
        for w in [0, 1, 1_000] {
            assert_eq!(counts(&weighted_rule(&[w]), 9, 50)["v0"], 50);
        }
    }

    #[test]
    fn equal_weights_at_a_tie_split_evenly() {
        let c = counts(&weighted_rule(&[1, 1]), 11, 10_000);
        assert!((c["v0"] as f64 / 10_000.0 - 0.5).abs() < 0.03, "{c:?}");
    }

    #[test]
    fn weight_never_beats_a_higher_priority() {
        let mut r = weighted_rule(&[1, 1]);
        r.variations[1].priority = 9;
        r.variations[1].weight = 0;
        r.variations[2].weight = 1_000;
        assert_eq!(counts(&r, 2, 200)["v0"], 200);
    }

    #[test]
    fn weight_is_ignored_by_the_default_look() {
        let mut r = weighted_rule(&[5]);
        r.variations[0].weight = 0;
        let e = sub_with_months(0);
        assert_eq!(r.pick_with(&e, &mut Rng::seeded(1)).unwrap().id, "default");
    }

    #[test]
    fn weights_above_the_limit_are_refused_and_old_rules_default_to_one() {
        let mut r = weighted_rule(&[1]);
        r.variations[1].weight = MAX_WEIGHT + 1;
        assert!(matches!(r.validate(), Err(RuleError::Weight(_))));
        r.variations[1].weight = MAX_WEIGHT;
        assert!(r.validate().is_ok());
        let mut j = serde_json::to_value(Variation::new("x", "t")).unwrap();
        j.as_object_mut().unwrap().remove("weight");
        assert_eq!(serde_json::from_value::<Variation>(j).unwrap().weight, 1);
        let mut j = serde_json::to_value(sub_rule()).unwrap();
        j.as_object_mut().unwrap().remove("interrupt");
        assert!(!serde_json::from_value::<AlertRule>(j).unwrap().interrupt);
    }

    #[test]
    fn select_with_uses_the_given_generator() {
        let rules = vec![weighted_rule(&[1, 1])];
        let e = sub_with_months(2);
        let a = select_with(&rules, &e, &mut Rng::seeded(4)).unwrap().1.id.clone();
        let b = select_with(&rules, &e, &mut Rng::seeded(4)).unwrap().1.id.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn validates_timing_tokens_custom_code_size_and_field_schema() {
        let mut r = sub_rule();
        r.variations[0].title = "Welcome {unknown_token}!".into();
        assert!(matches!(r.validate(), Err(RuleError::UnknownToken { ref token, .. }) if token == "unknown_token"));

        let mut r = sub_rule();
        r.variations[0].anim.in_ms = MAX_IN_MS + 1;
        assert!(matches!(r.validate(), Err(RuleError::Anim { .. })));

        let mut r = sub_rule();
        r.variations[0].duration_ms = 1_000;
        r.variations[0].anim.in_ms = 600;
        r.variations[0].anim.out_ms = 500;
        assert!(matches!(r.validate(), Err(RuleError::Anim { .. })));

        let mut r = sub_rule();
        r.variations[0].html = "<div>{bogus}</div>".into();
        assert!(matches!(r.validate(), Err(RuleError::UnknownToken { ref token, .. }) if token == "bogus"));

        let mut r = sub_rule();
        r.variations[0].html = "x".repeat(MAX_CUSTOM_HTML + 1);
        assert!(matches!(r.validate(), Err(RuleError::CustomHtmlTooLong { .. })));

        let mut r = sub_rule();
        r.variations[0].css = "@import url('https://evil.invalid/a.css');".into();
        assert!(matches!(r.validate(), Err(RuleError::CustomCss(_))));

        let mut r = sub_rule();
        r.variations[0].fields = vec![CustomField {
            key: "name".into(), // cannot shadow built-in KNOWN_VARS
            label: "Display".into(),
            kind: CustomFieldKind::Text,
            default: serde_json::json!("hi"),
            min: None,
            max: None,
            options: Vec::new(),
        }];
        assert!(matches!(r.validate(), Err(RuleError::CustomSchema { .. })));

        let mut r = sub_rule();
        r.variations[0].fields = vec![CustomField {
            key: "badge_color".into(),
            label: "Badge colour".into(),
            kind: CustomFieldKind::Color,
            default: serde_json::json!("red"), // must be hex or transparent
            min: None,
            max: None,
            options: Vec::new(),
        }];
        assert!(matches!(r.validate(), Err(RuleError::CustomSchema { .. })));

        let mut r = sub_rule();
        r.variations[0].fields = vec![CustomField {
            key: "badge_label".into(),
            label: "Badge label".into(),
            kind: CustomFieldKind::Text,
            default: serde_json::json!("VIP"),
            min: None,
            max: None,
            options: Vec::new(),
        }];
        r.variations[0].field_values.insert("undeclared".into(), serde_json::json!("oops"));
        assert!(matches!(r.validate(), Err(RuleError::CustomSchema { .. })));

        let mut r = sub_rule();
        r.variations[0].custom_enabled = true;
        r.variations[0].fields = vec![
            CustomField {
                key: "badge_label".into(),
                label: "Badge label".into(),
                kind: CustomFieldKind::Text,
                default: serde_json::json!("VIP"),
                min: None,
                max: None,
                options: Vec::new(),
            },
            CustomField {
                key: "glow_size".into(),
                label: "Glow px".into(),
                kind: CustomFieldKind::Number,
                default: serde_json::json!(12),
                min: Some(0.0),
                max: Some(40.0),
                options: Vec::new(),
            },
        ];
        r.variations[0].field_values.insert("badge_label".into(), serde_json::json!("Founder"));
        r.variations[0].html =
            "<div class='card'><span>{badge_label}</span><b>{name}</b><style>.card{color:#fff}</style></div>".into();
        r.variations[0].css = ".card { border-radius: 12px; }".into();
        assert!(r.validate().is_ok());

        let e = sub_with_months(3);
        let rendered = render("a-custom", &r, &r.variations[0], &e);
        assert!(rendered.anim.is_some());
        let custom = rendered.custom.expect("custom enabled renders custom payload");
        assert_eq!(custom.fields.get("badge_label"), Some(&serde_json::json!("Founder")));
        assert_eq!(custom.fields.get("glow_size"), Some(&serde_json::json!(12)));
        assert_eq!(custom.vars.get("name").map(String::as_str), Some("PixelPanda"));

        // Older Variation JSON without R25 fields deserializes cleanly with defaults
        let mut legacy = serde_json::to_value(Variation::new("legacy", "{name}")).unwrap();
        let obj = legacy.as_object_mut().unwrap();
        for key in ["anim", "custom_enabled", "html", "fields", "field_values"] {
            obj.remove(key);
        }
        let parsed: Variation = serde_json::from_value(legacy).unwrap();
        assert_eq!(parsed.anim, default_alert_anim());
        assert!(!parsed.custom_enabled);
        assert!(parsed.fields.is_empty());
    }

    fn custom_rule(html: &str) -> AlertRule {
        let mut r = sub_rule();
        r.variations[0].custom_enabled = true;
        r.variations[0].html = html.into();
        r
    }

    #[test]
    fn custom_html_cannot_carry_script_navigation_or_event_handlers() {
        for bad in [
            "<p>{message}</p><img src=\"data:,\" onerror=\"location.replace('https://evil.example/')\">",
            "<div onclick=\"x()\">hi</div>",
            "<DIV ONMOUSEOVER=x>hi</DIV>",
            "<script>fetch('https://evil.example')</script>",
            "<svg><script>1</script></svg>",
            "<meta http-equiv=\"refresh\" content=\"0;url=https://evil.example\">",
            "<base href=\"https://evil.example/\">",
            "<form action=\"https://evil.example\"><button>go</button></form>",
            "<iframe src=\"https://evil.example\"></iframe>",
            "<object data=\"x\"></object>",
            "<a href=\"javascript:alert(1)\">x</a>",
            "<a href=\" JaVa\tscript:alert(1)\">x</a>",
            "<a href=\"&#106;avascript:alert(1)\">x</a>",
            "<img src=\"https://evil.example/p.gif\">",
            "<img src=\"data:image/svg+xml,<svg onload=alert(1)>\">",
            "<audio src=\"data:audio/wav;base64,AAAA\" autoplay></audio>",
            "<svg><animate attributeName=\"href\" values=\"javascript:1\"/></svg>",
            "<div style=\"background:url(https://evil.example/a.png)\">x</div>",
            "<style>@import url(https://evil.example/a.css);</style>",
            "<style>.a{background:u\\72l(https://evil.example/a.png)}</style>",
        ] {
            let r = custom_rule(bad);
            assert!(
                matches!(r.validate(), Err(RuleError::CustomHtml { .. }) | Err(RuleError::CustomCss(_))),
                "should be refused: {bad}"
            );
        }
        for good in [
            "<div class='card'><span>{name}</span><b>{message}</b></div>",
            "<img src=\"https://cdn.discordapp.com/a.png\" alt=\"{name}\">",
            "<img src=\"data:image/png;base64,AAAA\" alt=\"\">",
            "<div style=\"color:#fff;background:linear-gradient(red,blue)\" title=\"{name}\">x</div>",
            "<style>.card{color:#fff}</style><p class=\"card\" data-who=\"{name}\">{{literal}}</p>",
            "<!-- a note --><p>{amount|default:\"0\"}</p>",
        ] {
            let r = custom_rule(good);
            assert!(r.validate().is_ok(), "should be accepted: {good}: {:?}", r.validate());
        }
    }

    #[test]
    fn tokens_only_fill_text_and_plain_attributes() {
        for bad in [
            "<div class=\"{name}\">x</div>",
            "<div class={name}>x</div>",
            "<img src=\"{message}\">",
            "<div style=\"color:{message}\">x</div>",
            "<div {name}>x</div>",
        ] {
            assert!(
                matches!(custom_rule(bad).validate(), Err(RuleError::CustomHtml { .. })),
                "token must be refused here: {bad}"
            );
        }
        // Tokens in a <style> block are not substituted, so they are not checked either.
        assert!(custom_rule("<style>.a{content:'{bogus}'}</style><p>{name}</p>").validate().is_ok());
        // An unknown token in text is still named.
        assert!(matches!(
            custom_rule("<p>{bogus}</p>").validate(),
            Err(RuleError::UnknownToken { ref token, .. }) if token == "bogus"
        ));
    }

    #[test]
    fn caps_looks_per_rule_and_the_words_sent_to_the_custom_frame() {
        let mut r = sub_rule();
        let extra = r.variations[0].clone();
        r.variations = (0..=MAX_VARIATIONS).map(|_| extra.clone()).collect();
        assert!(matches!(r.validate(), Err(RuleError::TooManyVariations)));

        let mut r = sub_rule();
        r.variations[0].custom_enabled = true;
        r.variations[0].html = "<p>{message}</p>".into();
        let mut e = sub_with_months(3);
        if let EventKind::Subscription { message, .. } = &mut e.kind {
            *message = Some(crate::event::Content::plain(&"w".repeat(50_000)));
        }
        let custom = render("a", &r, &r.variations[0], &e).custom.unwrap();
        assert!(custom.vars.values().all(|v| v.chars().count() <= MAX_CUSTOM_VAR_CHARS));
    }

    #[test]
    fn signed_picture_links_pass_and_brace_floods_do_not_stall_the_check() {
        let ok = custom_rule("<img src=\"https://cdn.discordapp.com/a/b.png?ex=65&is=64&hm=abc\" alt=\"\">");
        assert!(ok.validate().is_ok(), "{:?}", ok.validate());
        assert!(custom_rule("<a href=\"&#106;avascript:1\">x</a>").validate().is_err());
        // 20,000 opening braces and no closing one used to be a quadratic scan.
        let t = std::time::Instant::now();
        let _ = custom_rule(&"{".repeat(19_000)).validate();
        assert!(t.elapsed() < std::time::Duration::from_millis(500), "{:?}", t.elapsed());
        // The overlay does not read `{ name }`, so it is not a token here either.
        assert!(custom_rule("<p>{ bogus }</p>").validate().is_ok());
    }
}
