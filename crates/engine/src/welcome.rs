//! First-time and returning chatters: the dashboard's marks and the optional welcome.

use std::time::Duration;

use ferrocast_core::event::{Event, EventKind, Millis, Origin};
use ferrocast_core::welcome::{self, ChatterMark, WelcomeConfig};
use ferrocast_store::repo;
use ferrocast_store::repo::viewers::ChatTouch;

use crate::{Engine, EngineError, now};

/// At most one welcome this often: a raid of first-timers must not flood chat.
const WELCOME_GAP: Duration = Duration::from_secs(10);

impl Engine {
    pub fn welcome(&self) -> WelcomeConfig {
        self.state().welcome.clone()
    }

    /// Saves the fields that were sent; errors name the field.
    pub async fn patch_welcome(
        &self,
        patch: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<WelcomeConfig, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let mut cfg = crate::patch::merge(&self.welcome(), patch)?;
        cfg.template = cfg.template.trim().to_string();
        cfg.validate().map_err(EngineError::Invalid)?;
        let saved = cfg.clone();
        self.db(move |s| repo::settings::set(s, "welcome", &saved)).await?;
        self.state_mut().welcome = cfg.clone();
        Ok(cfg)
    }

    /// How the dashboard should mark this live message (first message ever, or back
    /// after a while). Only marks: the welcome is [`Engine::maybe_welcome`], sent once the
    /// message has passed moderation.
    pub(crate) fn chatter_mark(&self, e: &Event, touch: Option<ChatTouch>) -> Option<ChatterMark> {
        if !matches!(e.kind, EventKind::ChatMessage { .. }) || e.origin != Origin::Live {
            return None;
        }
        let t = touch?;
        let cfg = self.state().welcome.clone();
        welcome::mark(&cfg, t.first_message, t.previous_message_at.map(Millis), now())
    }

    /// The welcome for someone's first message ever, if it's turned on, the bot is on, it
    /// isn't panic mode, and none went out in the last 10 seconds. Call it only for a
    /// message that is shown.
    pub(crate) fn maybe_welcome(&self, e: &Event, mark: ChatterMark) {
        let EventKind::ChatMessage { actor, .. } = &e.kind else { return };
        let (cfg, bot_on, panic) = {
            let st = self.state();
            (st.welcome.clone(), st.settings.bot_enabled, st.panic_on)
        };
        let streamer = actor.roles.contains(ferrocast_core::event::Roles::BROADCASTER);
        if mark != ChatterMark::First || !cfg.enabled || !bot_on || panic || streamer || e.channel.is_some() {
            return;
        }
        let at = now();
        {
            let mut last = self.inner.last_welcome.lock().unwrap_or_else(|p| p.into_inner());
            if last.is_some_and(|l| at.0 - l.0 < WELCOME_GAP.as_millis() as i64) {
                return;
            }
            *last = Some(at);
        }
        let text = ferrocast_core::command::safe_reply(&cfg.render(&actor.display_name, e.platform.label()));
        if text.is_empty() {
            return;
        }
        let reply_to = e.source_id.clone();
        self.spawn_action(e.platform, "welcome a first-time chatter", move |c| {
            Box::pin(async move { c.send_chat(&text, reply_to.as_deref()).await })
        });
    }
}
