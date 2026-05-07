//! Overlays: one OBS browser source holding positioned widgets (API.md §4 Overlays).
//!
//! The widget `config` is a JSON object owned by the overlay bundle; core only checks the
//! parts that keep the server safe (known kinds, sane geometry, bounded size).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WidgetKind {
    Alerts,
    Chat,
    Events,
    Goal,
    Label,
    Viewers,
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
}

fn empty_object() -> Value {
    json!({})
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
        }
        Ok(())
    }

    /// Whether this overlay shows chat (so the server only streams chat where needed).
    pub fn wants_chat(&self) -> bool {
        self.widgets.iter().any(|w| w.kind == WidgetKind::Chat)
    }

    pub fn wants_alerts(&self) -> bool {
        self.widgets.iter().any(|w| w.kind == WidgetKind::Alerts)
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
    let alerts = Widget {
        id: "alerts".into(),
        kind: WidgetKind::Alerts,
        x: 560,
        y: 80,
        w: 800,
        h: 420,
        config: json!({ "theme": "molten", "rule_ids": null }),
    };
    let chat = Widget {
        id: "chat".into(),
        kind: WidgetKind::Chat,
        x: 40,
        y: 560,
        w: 520,
        h: 480,
        config: json!({ "platforms": [], "max_messages": 30, "fade_after_s": null, "show_badges": true,
                        "show_platform": true, "hide_commands": true, "hide_bots": true, "font_size": 20, "theme": "molten" }),
    };
    let events = Widget {
        id: "events".into(),
        kind: WidgetKind::Events,
        x: 1500,
        y: 40,
        w: 380,
        h: 320,
        config: json!({ "kinds": ["follow", "subscription", "gift_batch", "platform_gift", "paid_message", "tip", "raid"],
                        "max_items": 6, "theme": "molten" }),
    };
    match preset {
        Preset::Starter => vec![alerts, chat, events],
        Preset::AlertsOnly => vec![alerts],
        Preset::ChatOnly => vec![Widget { x: 40, y: 40, w: 520, h: 1000, ..chat }],
        Preset::Blank => vec![],
    }
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
        let many: Vec<Widget> = (0..41)
            .map(|i| Widget { id: i.to_string(), kind: WidgetKind::Label, x: 0, y: 0, w: 10, h: 10, config: json!({}) })
            .collect();
        assert!(matches!(overlay(many).validate(), Err(OverlayError::TooManyWidgets(_))));
    }
}
