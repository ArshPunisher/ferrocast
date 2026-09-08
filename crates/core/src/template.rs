//! Text templates for alerts and bot replies: `"{name} just subscribed for {months} months!"`.
//!
//! Deliberately tiny: variables and a few formatting filters, no logic and no code. The
//! output is plain text; overlays put it into the page with `textContent`, so a viewer
//! named `<img onerror=…>` shows up as those characters, never as markup (R-37).

use std::collections::BTreeMap;

use crate::event::{Event, EventKind};

/// Values available to a template.
pub type Vars = BTreeMap<String, String>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Text(String),
    Var { name: String, filters: Vec<Filter> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Filter {
    Upper,
    Lower,
    Title,
    Truncate(usize),
    Default(String),
}

/// A parsed template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    pieces: Vec<Piece>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    #[error("a '{{' at character {0} is never closed")]
    Unclosed(usize),
    #[error("'{{{0}}}' isn't a valid variable name")]
    BadName(String),
    #[error("'{0}' isn't a known filter (use upper, lower, title, truncate:N or default:text)")]
    BadFilter(String),
    #[error("the template is longer than {0} characters")]
    TooLong(usize),
}

/// Longest template we accept. Alert text that long is a mistake or an attack.
pub const MAX_TEMPLATE_CHARS: usize = 2_000;
/// Longest rendered output. A viewer can't blow up an alert with a 10 000-character name.
pub const MAX_OUTPUT_CHARS: usize = 4_000;

impl Template {
    /// Parses `{var}`, `{var|filter|filter:arg}` and `{{` / `}}` for literal braces.
    pub fn parse(src: &str) -> Result<Template, TemplateError> {
        if src.chars().count() > MAX_TEMPLATE_CHARS {
            return Err(TemplateError::TooLong(MAX_TEMPLATE_CHARS));
        }
        let mut pieces = Vec::new();
        let mut text = String::new();
        let chars: Vec<char> = src.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '{' if chars.get(i + 1) == Some(&'{') => {
                    text.push('{');
                    i += 2;
                }
                '}' if chars.get(i + 1) == Some(&'}') => {
                    text.push('}');
                    i += 2;
                }
                '{' => {
                    let close = chars[i + 1..].iter().position(|&c| c == '}').ok_or(TemplateError::Unclosed(i))?;
                    let inner: String = chars[i + 1..i + 1 + close].iter().collect();
                    if !text.is_empty() {
                        pieces.push(Piece::Text(std::mem::take(&mut text)));
                    }
                    pieces.push(parse_var(&inner)?);
                    i += close + 2;
                }
                c => {
                    text.push(c);
                    i += 1;
                }
            }
        }
        if !text.is_empty() {
            pieces.push(Piece::Text(text));
        }
        Ok(Template { pieces })
    }

    /// Variable names the template uses, so the editor can warn about unknown ones.
    pub fn variables(&self) -> Vec<&str> {
        self.pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Var { name, .. } => Some(name.as_str()),
                Piece::Text(_) => None,
            })
            .collect()
    }

    /// Renders with `vars`. A missing variable renders empty unless it has a `default:`
    /// filter; the editor already warned about it.
    pub fn render(&self, vars: &Vars) -> String {
        let mut out = String::new();
        for piece in &self.pieces {
            match piece {
                Piece::Text(t) => out.push_str(t),
                Piece::Var { name, filters } => {
                    let mut value = vars.get(name).cloned().unwrap_or_default();
                    for f in filters {
                        value = apply(f, value);
                    }
                    out.push_str(&value);
                }
            }
            if out.chars().count() > MAX_OUTPUT_CHARS {
                return out.chars().take(MAX_OUTPUT_CHARS).collect();
            }
        }
        out
    }

    /// Renders like [`Template::render`], but keeps track of which text came from the
    /// variables in `highlight`, so an alert design can colour the name and amount. Neighbours
    /// with the same flag are merged; the joined text is exactly what `render` returns.
    pub fn render_parts(&self, vars: &Vars, highlight: &[&str]) -> Vec<TextPart> {
        let mut parts: Vec<TextPart> = Vec::new();
        let mut total = 0usize;
        for piece in &self.pieces {
            let (mut text, hl) = match piece {
                Piece::Text(t) => (t.clone(), false),
                Piece::Var { name, filters } => {
                    let mut value = vars.get(name).cloned().unwrap_or_default();
                    for f in filters {
                        value = apply(f, value);
                    }
                    (value, highlight.contains(&name.as_str()))
                }
            };
            let n = text.chars().count();
            if total + n > MAX_OUTPUT_CHARS {
                text = text.chars().take(MAX_OUTPUT_CHARS - total).collect();
            }
            total += text.chars().count();
            if text.is_empty() {
                continue;
            }
            match parts.last_mut() {
                Some(last) if last.hl == hl => last.text.push_str(&text),
                _ => parts.push(TextPart { text, hl }),
            }
            if total >= MAX_OUTPUT_CHARS {
                break;
            }
        }
        parts
    }
}

/// A run of rendered text; `hl` marks text that came from a highlighted variable.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TextPart {
    pub text: String,
    pub hl: bool,
}

fn parse_var(inner: &str) -> Result<Piece, TemplateError> {
    let mut parts = inner.split('|');
    let name = parts.next().unwrap_or("").trim().to_string();
    // `count:deaths` reads a named counter (see `chatbot`); the colon is only allowed after
    // `count`, so `{name:x}` stays a mistake the editor reports.
    let named_counter = name.strip_prefix("count:").is_some_and(|n| !n.is_empty());
    let plain = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
    if name.is_empty() || !(plain(&name) || named_counter && plain(&name[6..])) {
        return Err(TemplateError::BadName(inner.to_string()));
    }
    let mut filters = Vec::new();
    for raw in parts {
        let raw = raw.trim();
        let (fname, arg) = match raw.split_once(':') {
            Some((n, a)) => (n.trim(), Some(a)),
            None => (raw, None),
        };
        filters.push(match (fname, arg) {
            ("upper", None) => Filter::Upper,
            ("lower", None) => Filter::Lower,
            ("title", None) => Filter::Title,
            ("truncate", Some(n)) => {
                Filter::Truncate(n.trim().parse().map_err(|_| TemplateError::BadFilter(raw.to_string()))?)
            }
            ("default", Some(text)) => Filter::Default(text.trim_matches('"').to_string()),
            _ => return Err(TemplateError::BadFilter(raw.to_string())),
        });
    }
    Ok(Piece::Var { name, filters })
}

fn apply(f: &Filter, value: String) -> String {
    match f {
        Filter::Upper => value.to_uppercase(),
        Filter::Lower => value.to_lowercase(),
        Filter::Title => {
            let mut out = String::with_capacity(value.len());
            let mut start = true;
            for c in value.chars() {
                if start {
                    out.extend(c.to_uppercase());
                } else {
                    out.push(c);
                }
                start = c.is_whitespace();
            }
            out
        }
        Filter::Truncate(n) => {
            if value.chars().count() > *n {
                let mut t: String = value.chars().take(n.saturating_sub(1)).collect();
                t.push('…');
                t
            } else {
                value
            }
        }
        Filter::Default(d) => {
            if value.trim().is_empty() {
                d.clone()
            } else {
                value
            }
        }
    }
}

/// The standard variables for an event. These names are the public template API: the
/// dashboard lists them, so renaming one is a breaking change.
pub fn event_vars(event: &Event) -> Vars {
    let mut v = Vars::new();
    v.insert("platform".into(), event.platform.label().into());
    if let Some(actor) = event.kind.actor() {
        v.insert("name".into(), actor.display_name.clone());
        v.insert("user".into(), actor.login.clone().unwrap_or_else(|| actor.display_name.clone()));
    }
    match &event.kind {
        EventKind::Subscription { tier, months, message, gifted_by, .. } => {
            v.insert("months".into(), months.to_string());
            v.insert("months_text".into(), plural(u64::from(*months), "month", "months"));
            v.insert("tier".into(), tier_label(tier));
            if let Some(m) = message {
                v.insert("message".into(), m.text.clone());
            }
            if let Some(g) = gifted_by {
                v.insert("gifter".into(), g.display_name.clone());
            }
        }
        EventKind::GiftBatch { tier, count, .. } => {
            v.insert("count".into(), count.to_string());
            v.insert("count_text".into(), plural(u64::from(*count), "sub", "subs"));
            v.insert("amount".into(), count.to_string());
            v.insert("tier".into(), tier_label(tier));
        }
        EventKind::PlatformGift { value, message, .. } => {
            v.insert("amount".into(), value.amount.to_string());
            v.insert("value".into(), value.display());
            v.insert("unit".into(), value.unit.label(value.amount));
            if let Some(m) = message {
                v.insert("message".into(), m.text.clone());
            }
        }
        EventKind::PaidMessage { money, content, .. } => {
            v.insert("amount".into(), money.display());
            v.insert("currency".into(), money.currency.clone());
            if let Some(c) = content {
                v.insert("message".into(), c.text.clone());
            }
        }
        EventKind::Tip { money, message, .. } => {
            v.insert("amount".into(), money.display());
            v.insert("currency".into(), money.currency.clone());
            if let Some(m) = message {
                v.insert("message".into(), m.clone());
            }
        }
        EventKind::Raid { viewers, .. } => {
            v.insert("viewers".into(), viewers.to_string());
            v.insert("viewers_text".into(), plural(u64::from(*viewers), "viewer", "viewers"));
            v.insert("amount".into(), viewers.to_string());
        }
        EventKind::RewardRedemption { reward, cost, input, .. } => {
            v.insert("reward".into(), reward.clone());
            v.insert("cost".into(), cost.to_string());
            if let Some(i) = input {
                v.insert("message".into(), i.clone());
            }
        }
        EventKind::ChatMessage { content, .. } => {
            v.insert("message".into(), content.text.clone());
        }
        _ => {}
    }
    v
}

fn tier_label(t: &crate::event::Tier) -> String {
    use crate::event::Tier;
    match t {
        Tier::Prime => "Prime".into(),
        Tier::Tier1 => "Tier 1".into(),
        Tier::Tier2 => "Tier 2".into(),
        Tier::Tier3 => "Tier 3".into(),
        Tier::Level { name, .. } => name.clone(),
    }
}

/// "1 month", "12 months": a count with the word that fits it.
pub fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Templates written as "{months} months" (the old default, and what most people type)
/// read "1 months" for a first sub. Each pair is rewritten to its `_text` variable before
/// rendering, so existing templates say "1 month" without being edited.
const COUNTED_WORDS: [(&str, &str); 3] =
    [("{months} months", "{months_text}"), ("{count} subs", "{count_text}"), ("{viewers} viewers", "{viewers_text}")];

/// Rewrites "{months} months" and friends to their singular-aware variables.
pub fn with_plurals(src: &str) -> std::borrow::Cow<'_, str> {
    if !COUNTED_WORDS.iter().any(|(from, _)| src.contains(from)) {
        return std::borrow::Cow::Borrowed(src);
    }
    let mut out = src.to_string();
    for (from, to) in COUNTED_WORDS {
        out = out.replace(from, to);
    }
    std::borrow::Cow::Owned(out)
}

/// Every variable name any event can set, for the editor's autocomplete.
pub const KNOWN_VARS: [&str; 18] = [
    "name",
    "user",
    "platform",
    "amount",
    "currency",
    "message",
    "months",
    "months_text",
    "tier",
    "gifter",
    "count",
    "count_text",
    "value",
    "unit",
    "viewers",
    "viewers_text",
    "reward",
    "cost",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Platform;
    use crate::sample::any_sample;

    fn vars(pairs: &[(&str, &str)]) -> Vars {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn renders_variables_and_filters() {
        let t = Template::parse("{name|upper} gave {amount}! {message|default:\"no message\"}").unwrap();
        assert_eq!(t.render(&vars(&[("name", "asha"), ("amount", "₹500")])), "ASHA gave ₹500! no message");
        let t = Template::parse("{name|title}").unwrap();
        assert_eq!(t.render(&vars(&[("name", "pixel panda")])), "Pixel Panda");
    }

    #[test]
    fn counts_read_right_for_one() {
        assert_eq!(plural(1, "month", "months"), "1 month");
        assert_eq!(plural(0, "month", "months"), "0 months");
        assert_eq!(plural(12, "month", "months"), "12 months");
        let v = vars(&[("months", "1"), ("months_text", "1 month"), ("count", "1"), ("count_text", "1 sub")]);
        let t = Template::parse(&with_plurals("{name} subscribed for {months} months! Gifted {count} subs")).unwrap();
        assert_eq!(t.render(&v), " subscribed for 1 month! Gifted 1 sub");
        // Anything else is left exactly as written.
        assert_eq!(with_plurals("{months} mo"), "{months} mo");
        assert!(matches!(with_plurals("plain"), std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn literal_braces() {
        let t = Template::parse("{{not a var}} {name}").unwrap();
        assert_eq!(t.render(&vars(&[("name", "x")])), "{not a var} x");
    }

    #[test]
    fn missing_variables_render_empty() {
        let t = Template::parse("hi {nobody}!").unwrap();
        assert_eq!(t.render(&Vars::new()), "hi !");
        assert_eq!(t.variables(), vec!["nobody"]);
    }

    #[test]
    fn rejects_broken_templates() {
        assert_eq!(Template::parse("hello {name"), Err(TemplateError::Unclosed(6)));
        assert!(matches!(Template::parse("{}"), Err(TemplateError::BadName(_))));
        assert!(matches!(Template::parse("{na me}"), Err(TemplateError::BadName(_))));
        assert!(matches!(Template::parse("{name|explode}"), Err(TemplateError::BadFilter(_))));
        assert!(matches!(Template::parse("{name|truncate:abc}"), Err(TemplateError::BadFilter(_))));
        assert!(matches!(Template::parse(&"a".repeat(MAX_TEMPLATE_CHARS + 1)), Err(TemplateError::TooLong(_))));
    }

    #[test]
    fn viewer_text_stays_text() {
        let t = Template::parse("{name}").unwrap();
        let evil = "<img src=x onerror=alert(1)>{message}";
        assert_eq!(t.render(&vars(&[("name", evil), ("message", "boom")])), evil, "no second expansion");
    }

    #[test]
    fn output_is_capped() {
        let t = Template::parse("{name}{name}").unwrap();
        let huge = "x".repeat(MAX_OUTPUT_CHARS);
        assert_eq!(t.render(&vars(&[("name", &huge)])).chars().count(), MAX_OUTPUT_CHARS);
    }

    #[test]
    fn truncate_counts_characters_not_bytes() {
        let t = Template::parse("{name|truncate:4}").unwrap();
        assert_eq!(t.render(&vars(&[("name", "नमस्ते दुनिया")])), "नमस…");
        assert_eq!(t.render(&vars(&[("name", "abc")])), "abc");
    }

    #[test]
    fn every_sample_has_name_and_platform() {
        for kind in crate::sample::TESTABLE {
            let e = any_sample(kind, crate::event::Millis(0)).unwrap();
            let v = event_vars(&e);
            assert!(v.contains_key("name") && v.contains_key("platform"), "{kind}");
            for key in v.keys() {
                assert!(KNOWN_VARS.contains(&key.as_str()), "{key} missing from KNOWN_VARS");
            }
        }
        let tip = any_sample("tip", crate::event::Millis(0)).unwrap();
        assert_eq!(event_vars(&tip).get("amount").map(String::as_str), Some("₹500"));
        assert_eq!(tip.platform, Platform::Tips);
    }

    #[test]
    fn parts_mark_highlighted_variables_and_join_to_render() {
        let t = Template::parse("{name|upper} tipped {amount}! {missing}{{x}}").unwrap();
        let mut v = Vars::new();
        v.insert("name".into(), "kavya".into());
        v.insert("amount".into(), "₹750".into());
        let parts = t.render_parts(&v, &["name", "amount"]);
        let joined: String = parts.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(joined, t.render(&v));
        assert_eq!(parts[0], TextPart { text: "KAVYA".into(), hl: true });
        assert_eq!(parts[1], TextPart { text: " tipped ".into(), hl: false });
        assert_eq!(parts[2], TextPart { text: "₹750".into(), hl: true });
        assert_eq!(parts[3], TextPart { text: "! {x}".into(), hl: false }, "empty values vanish, neighbours merge");
        let long = Template::parse("{name}{name}").unwrap();
        v.insert("name".into(), "é".repeat(MAX_OUTPUT_CHARS));
        let capped = long.render_parts(&v, &["name"]);
        assert_eq!(capped.iter().map(|p| p.text.chars().count()).sum::<usize>(), MAX_OUTPUT_CHARS);
    }
}
