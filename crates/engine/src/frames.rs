//! Frames sent to the dashboard's live socket and to overlays (API.md §5, §6).
//!
//! A frame is serialised once and shared by every socket as an `Arc<str>`, so a chat
//! message to ten overlays costs one serialisation.

use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};

/// Which overlays need a frame. Sockets check this against their overlay's widgets so a
/// chat-free overlay never receives chat (and OBS's browser does less work).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Audience {
    All,
    /// Overlays with a chat widget.
    Chat,
    /// Overlays with an alerts or events widget.
    Alerts,
    /// One overlay (config changes).
    Overlay(String),
}

#[derive(Debug, Clone)]
pub struct OverlayFrame {
    pub audience: Audience,
    pub seq: u64,
    pub text: Arc<str>,
}

impl OverlayFrame {
    pub fn new(audience: Audience, seq: u64, kind: &str, data: &impl Serialize) -> Self {
        let text = envelope(Some(seq), kind, data);
        OverlayFrame { audience, seq, text }
    }
}

/// A frame for the dashboard's live socket.
#[derive(Debug, Clone)]
pub struct LiveFrame {
    pub text: Arc<str>,
}

impl LiveFrame {
    pub fn new(kind: &str, data: &impl Serialize) -> Self {
        LiveFrame { text: envelope(None, kind, data) }
    }
}

/// `{ v: 1, seq?, type, data }`. Serialising our own types can't fail in practice; if it
/// ever does, the frame carries `null` data rather than taking the socket down.
pub fn envelope(seq: Option<u64>, kind: &str, data: &impl Serialize) -> Arc<str> {
    let data = serde_json::to_value(data).unwrap_or(Value::Null);
    let v = match seq {
        Some(seq) => json!({ "v": 1, "seq": seq, "type": kind, "data": data }),
        None => json!({ "v": 1, "type": kind, "data": data }),
    };
    Arc::from(v.to_string())
}

/// A message for the dashboard's notice area.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Notice {
    pub level: NoticeLevel,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<NoticeAction>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NoticeAction {
    pub label: String,
    pub href: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_shape() {
        let t = envelope(Some(7), "alert_hide", &json!({ "alert_id": "a" }));
        let v: Value = serde_json::from_str(&t).unwrap();
        assert_eq!(v, json!({ "v": 1, "seq": 7, "type": "alert_hide", "data": { "alert_id": "a" } }));
        let t = envelope(None, "notice", &"x");
        assert!(!t.contains("seq"));
    }
}
