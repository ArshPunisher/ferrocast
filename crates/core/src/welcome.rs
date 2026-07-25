//! First-time chatters and people coming back: how the dashboard marks them, and the
//! optional welcome message (off by default: a bot greeting everyone can feel like spam).

use serde::{Deserialize, Serialize};

use crate::event::Millis;
use crate::template::{Template, Vars};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WelcomeConfig {
    /// Post `template` in chat for someone's first message ever.
    pub enabled: bool,
    pub template: String,
    /// Mark people chatting again after `returning_days` away.
    pub returning_badge: bool,
    pub returning_days: u32,
}

impl Default for WelcomeConfig {
    fn default() -> Self {
        WelcomeConfig {
            enabled: false,
            template: "Welcome to the stream, {user}! Glad you're here.".into(),
            returning_badge: true,
            returning_days: 30,
        }
    }
}

impl WelcomeConfig {
    pub fn validate(&self) -> Result<(), String> {
        let t = self.template.trim();
        if t.is_empty() || t.chars().count() > 300 {
            return Err("template: write a welcome of 1 to 300 characters.".into());
        }
        Template::parse(t).map_err(|e| format!("template: {e}."))?;
        if !(7..=365).contains(&self.returning_days) {
            return Err("returning_days: must be between 7 and 365 days.".into());
        }
        Ok(())
    }

    /// The welcome for `name` on `platform`.
    pub fn render(&self, name: &str, platform: &str) -> String {
        let mut vars = Vars::new();
        vars.insert("user".into(), name.to_string());
        vars.insert("name".into(), name.to_string());
        vars.insert("platform".into(), platform.to_string());
        Template::parse(&self.template).map(|t| t.render(&vars)).unwrap_or_default()
    }
}

/// How the dashboard marks one chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChatterMark {
    /// Their first message ever (on any linked account).
    First,
    /// Chatting again after `days` away.
    Returning { days: u32 },
}

/// Marks a message from what was known before it.
pub fn mark(cfg: &WelcomeConfig, first: bool, previous: Option<Millis>, now: Millis) -> Option<ChatterMark> {
    if first {
        return Some(ChatterMark::First);
    }
    let prev = previous?;
    let days = now.seconds_since(prev) / 86_400;
    (cfg.returning_badge && days >= i64::from(cfg.returning_days))
        .then(|| ChatterMark::Returning { days: u32::try_from(days).unwrap_or(u32::MAX) })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400_000;

    #[test]
    fn marks_first_and_returning_chatters() {
        let c = WelcomeConfig::default();
        assert_eq!(mark(&c, true, None, Millis(0)), Some(ChatterMark::First));
        assert_eq!(mark(&c, false, Some(Millis(0)), Millis(29 * DAY)), None);
        assert_eq!(mark(&c, false, Some(Millis(0)), Millis(45 * DAY)), Some(ChatterMark::Returning { days: 45 }));
        assert_eq!(mark(&c, false, None, Millis(45 * DAY)), None);
        let off = WelcomeConfig { returning_badge: false, ..c };
        assert_eq!(mark(&off, false, Some(Millis(0)), Millis(45 * DAY)), None);
    }

    #[test]
    fn validation_and_rendering() {
        let c = WelcomeConfig::default();
        assert!(c.validate().is_ok());
        assert!(!c.enabled, "off by default");
        assert_eq!(c.render("Sam", "Kick"), "Welcome to the stream, Sam! Glad you're here.");
        let bad = |c: WelcomeConfig| c.validate().unwrap_err();
        assert!(bad(WelcomeConfig { template: " ".into(), ..c.clone() }).starts_with("template:"));
        assert!(bad(WelcomeConfig { template: "{oops".into(), ..c.clone() }).starts_with("template:"));
        assert!(bad(WelcomeConfig { template: "x".repeat(301), ..c.clone() }).starts_with("template:"));
        assert!(bad(WelcomeConfig { returning_days: 6, ..c.clone() }).starts_with("returning_days:"));
        assert!(bad(WelcomeConfig { returning_days: 366, ..c }).starts_with("returning_days:"));
    }
}
