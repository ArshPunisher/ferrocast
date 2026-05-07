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
}

fn parse_var(inner: &str) -> Result<Piece, TemplateError> {
    let mut parts = inner.split('|');
    let name = parts.next().unwrap_or("").trim().to_string();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') {
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

/// Every variable name any event can set, for the editor's autocomplete.
pub const KNOWN_VARS: [&str; 15] = [
    "name", "user", "platform", "amount", "currency", "message", "months", "tier", "gifter", "count", "value", "unit",
    "viewers", "reward", "cost",
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
}
