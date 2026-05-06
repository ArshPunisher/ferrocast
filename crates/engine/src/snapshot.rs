//! What an overlay receives first (API.md §6 `snapshot`) and overlay CRUD for the API.

use ferrocast_core::overlay::{Overlay, Preset, WidgetKind};
use ferrocast_core::template::{Template, Vars};
use ferrocast_store::repo;
use serde::Serialize;
use serde_json::{Value, json};

use crate::frames::{Audience, OverlayFrame};
use crate::{Engine, EngineError, platform_key};

/// An overlay as the dashboard sees it: the URL instead of the bare token.
#[derive(Debug, Clone, Serialize)]
pub struct OverlayView {
    pub id: String,
    pub name: String,
    pub url: String,
    pub width: i32,
    pub height: i32,
    pub widgets: Vec<ferrocast_core::overlay::Widget>,
    pub updated_at: i64,
}

const ALERTABLE: [&str; 7] = ["follow", "subscription", "gift_batch", "platform_gift", "paid_message", "tip", "raid"];

impl Engine {
    pub fn overlay_view(&self, o: &Overlay) -> OverlayView {
        OverlayView {
            id: o.id.clone(),
            name: o.name.clone(),
            url: format!("{}/o/{}/", self.base_url(), o.token),
            width: o.width,
            height: o.height,
            widgets: o.widgets.clone(),
            updated_at: o.updated_at,
        }
    }

    pub async fn overlays(&self) -> Result<Vec<OverlayView>, EngineError> {
        let list = self.db(repo::overlays::list).await?;
        Ok(list.iter().map(|o| self.overlay_view(o)).collect())
    }

    pub async fn overlay(&self, id: &str) -> Result<Overlay, EngineError> {
        let oid = id.to_string();
        self.db(move |s| repo::overlays::get(s, &oid)).await
    }

    pub async fn create_overlay(&self, name: &str, preset: Preset) -> Result<OverlayView, EngineError> {
        let n = name.to_string();
        let o = self.db(move |s| repo::overlays::create(s, &n, preset)).await?;
        Ok(self.overlay_view(&o))
    }

    /// Saves an edited overlay and pushes the new layout to its open pages.
    pub async fn update_overlay(&self, o: Overlay) -> Result<OverlayView, EngineError> {
        o.validate().map_err(|e| EngineError::Invalid(e.to_string()))?;
        let saved = self.db(move |s| repo::overlays::update(s, &o)).await?;
        let view = self.overlay_view(&saved);
        self.send_overlay(OverlayFrame::new(
            Audience::Overlay(saved.id.clone()),
            self.next_seq(),
            "config",
            &view_public(&view),
        ));
        Ok(view)
    }

    pub async fn regenerate_overlay_url(&self, id: &str) -> Result<OverlayView, EngineError> {
        let oid = id.to_string();
        let o = self.db(move |s| repo::overlays::regenerate_token(s, &oid)).await?;
        // Pages still open on the old URL must stop at once.
        self.revoke(&o.id);
        Ok(self.overlay_view(&o))
    }

    pub async fn delete_overlay(&self, id: &str) -> Result<(), EngineError> {
        let oid = id.to_string();
        self.db(move |s| repo::overlays::delete(s, &oid)).await?;
        self.revoke(id);
        Ok(())
    }

    fn revoke(&self, overlay_id: &str) {
        self.send_overlay(OverlayFrame::new(
            Audience::Overlay(overlay_id.to_string()),
            self.next_seq(),
            "revoked",
            &json!({}),
        ));
    }

    pub async fn overlay_by_token(&self, token: &str) -> Result<Option<Overlay>, EngineError> {
        let t = token.to_string();
        self.db(move |s| repo::overlays::by_token(s, &t)).await
    }

    /// The first frame for an overlay page: layout plus current state, so nothing is blank.
    pub async fn overlay_snapshot(&self, o: &Overlay) -> Result<Value, EngineError> {
        let events = if o.wants_alerts() || o.widgets.iter().any(|w| w.kind == WidgetKind::Events) {
            let kinds: Vec<String> = ALERTABLE.iter().map(|s| s.to_string()).collect();
            let mut list = self
                .db(move |s| repo::events::recent_visible(s, None, 40, &kinds))
                .await?
                .into_iter()
                .map(|(_, e)| e)
                .collect::<Vec<_>>();
            // Same rule as the live path: only live events that could alert (a pending or
            // refunded tip, or a backfilled follow, never shows on stream).
            list.retain(|e| e.origin == ferrocast_core::event::Origin::Live && e.kind.is_alertable());
            list.truncate(25);
            list
        } else {
            Vec::new()
        };
        let chat: Vec<_> = if o.wants_chat() {
            let mut c: Vec<_> = self
                .db(|s| repo::events::recent_visible(s, None, 50, &["chat_message".to_string()]))
                .await?
                .into_iter()
                .map(|(_, e)| e)
                .collect();
            c.retain(|e| !e.source_id.as_deref().is_some_and(|id| self.is_hidden(e.platform, id)));
            c.reverse();
            c
        } else {
            Vec::new()
        };
        let mut events = events;
        events.reverse();
        let (goals, viewers, labels) = {
            let st = self.state();
            let viewers: serde_json::Map<String, Value> =
                st.viewers.iter().map(|(p, c)| (platform_key(*p).to_string(), json!(c))).collect();
            let labels: serde_json::Map<String, Value> = o
                .widgets
                .iter()
                .filter(|w| w.kind == WidgetKind::Label)
                .map(|w| {
                    let source = w.config.get("source").and_then(Value::as_str).unwrap_or("");
                    let tpl = w.config.get("template").and_then(Value::as_str).unwrap_or("{value}");
                    let mut vars = Vars::new();
                    vars.insert("value".into(), st.session.label(source));
                    let text = Template::parse(tpl).map(|t| t.render(&vars)).unwrap_or_default();
                    (w.id.clone(), json!(text))
                })
                .collect();
            (st.goals.clone(), viewers, labels)
        };
        let view = self.overlay_view(o);
        Ok(json!({
            "overlay": view_public(&view),
            "state": { "goals": goals, "labels": labels, "viewers": viewers, "events": events, "chat": chat },
        }))
    }
}

/// Overlay pages don't need to see their own URL (it contains the token).
fn view_public(v: &OverlayView) -> Value {
    json!({ "id": v.id, "name": v.name, "width": v.width, "height": v.height, "widgets": v.widgets, "updated_at": v.updated_at })
}
