//! In-memory state the pipeline reads on every event, loaded from the store at start and
//! refreshed whenever the dashboard saves something.

use std::collections::{HashMap, VecDeque};

use ferrocast_core::alert::AlertRule;
use ferrocast_core::command::{Command, Timer};
use ferrocast_core::event::{ConnectorStatus, Event, EventKind, Millis, Platform};
use ferrocast_core::filter::FilterConfig;
use ferrocast_core::goal::Goal;
use ferrocast_core::moderation::{BanList, PanicConfig};
use ferrocast_core::money::Money;
use ferrocast_core::tts::TtsPolicy;
use serde::{Deserialize, Serialize};

/// Settings the pipeline needs, edited from the dashboard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineSettings {
    /// Days of chat to keep in the log (alerts and tips are kept).
    pub keep_chat_days: u32,
    /// Fire alerts for test events from the dashboard's test buttons.
    pub test_alerts: bool,
    /// Turn chat replies from the bot on or off as a whole.
    pub bot_enabled: bool,
    /// Ban accounts on the shared ban list automatically when they chat.
    pub auto_ban_on_sight: bool,
}

impl Default for EngineSettings {
    fn default() -> Self {
        EngineSettings { keep_chat_days: 30, test_alerts: true, bot_enabled: true, auto_ban_on_sight: true }
    }
}

/// Everything the pipeline consults, kept together under one lock.
#[derive(Debug, Default)]
pub struct State {
    pub settings: EngineSettings,
    pub rules: Vec<AlertRule>,
    pub filter: FilterConfig,
    pub tts: TtsPolicy,
    pub commands: Vec<Command>,
    pub timers: Vec<Timer>,
    pub bans: BanList,
    pub panic: PanicConfig,
    pub panic_on: bool,
    pub tts_paused: bool,
    pub goals: Vec<Goal>,
    pub live: HashMap<Platform, bool>,
    pub viewers: HashMap<Platform, u64>,
    pub connectors: HashMap<String, ConnectorView>,
    pub session: Session,
    /// Why recent events did or didn't alert, newest last (bounded).
    pub why: VecDeque<(String, Why)>,
}

/// Health of one connection as the dashboard shows it (API.md `ConnectorState`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorView {
    pub state: String,
    pub account: Option<AccountView>,
    pub detail: Option<String>,
    pub last_event_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountView {
    pub name: String,
    pub avatar_url: Option<String>,
}

impl ConnectorView {
    pub fn off() -> Self {
        ConnectorView { state: "off".into(), account: None, detail: None, last_event_at: None }
    }

    pub fn apply(&mut self, status: &ConnectorStatus) {
        let (state, detail) = match status {
            ConnectorStatus::Connected => ("connected", None),
            ConnectorStatus::Reconnecting { attempt } => ("reconnecting", Some(format!("Attempt {attempt}"))),
            ConnectorStatus::Disconnected { reason } => ("disconnected", Some(reason.clone())),
            ConnectorStatus::SignInNeeded => ("sign_in_needed", Some("Sign in again to keep events coming.".into())),
            ConnectorStatus::ScopesMissing { scopes } => {
                ("scopes_missing", Some(format!("Needs permission: {}", scopes.join(", "))))
            }
        };
        self.state = state.into();
        self.detail = detail;
    }
}

/// The dashboard's answer to "why didn't this alert fire?"
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Why {
    pub alerted: bool,
    pub reasons: Vec<String>,
}

pub const WHY_KEEP: usize = 500;

impl State {
    pub fn remember_why(&mut self, event_id: &str, why: Why) {
        self.why.push_back((event_id.to_string(), why));
        while self.why.len() > WHY_KEEP {
            self.why.pop_front();
        }
    }

    pub fn why(&self, event_id: &str) -> Option<&Why> {
        self.why.iter().rev().find(|(id, _)| id == event_id).map(|(_, w)| w)
    }
}

/// This stream's numbers, for labels and the home screen. Reset when a stream starts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub started_at: Option<Millis>,
    pub follows: u32,
    pub subs: u32,
    pub latest_follower: Option<String>,
    pub latest_sub: Option<String>,
    pub latest_tip: Option<(String, Money)>,
    pub top_tip: Option<(String, Money)>,
    pub chat_lines: HashMap<Platform, u32>,
}

impl Session {
    /// Updates counts from a live event.
    pub fn record(&mut self, event: &Event) {
        match &event.kind {
            EventKind::Follow { actor } => {
                self.follows += 1;
                self.latest_follower = Some(actor.display_name.clone());
            }
            EventKind::Subscription { actor, gifted_by: None, .. } => {
                self.subs += 1;
                self.latest_sub = Some(actor.display_name.clone());
            }
            EventKind::GiftBatch { gifter, count, .. } => {
                self.subs += count;
                self.latest_sub = Some(gifter.display_name.clone());
            }
            EventKind::Tip { actor, money, .. } | EventKind::PaidMessage { actor, money, .. } => {
                if event.kind.is_alertable() {
                    self.latest_tip = Some((actor.display_name.clone(), money.clone()));
                    let beats = match &self.top_tip {
                        Some((_, top)) => top.currency == money.currency && money.minor > top.minor,
                        None => true,
                    };
                    if beats {
                        self.top_tip = Some((actor.display_name.clone(), money.clone()));
                    }
                }
            }
            EventKind::ChatMessage { .. } => {
                *self.chat_lines.entry(event.platform).or_default() += 1;
            }
            _ => {}
        }
    }

    /// Text for a label widget source (API.md widget `label`).
    pub fn label(&self, source: &str) -> String {
        match source {
            "latest_follower" => self.latest_follower.clone().unwrap_or_default(),
            "latest_sub" => self.latest_sub.clone().unwrap_or_default(),
            "latest_tip" => self.latest_tip.as_ref().map(|(n, m)| format!("{n} {}", m.display())).unwrap_or_default(),
            "top_tip_session" => self.top_tip.as_ref().map(|(n, m)| format!("{n} {}", m.display())).unwrap_or_default(),
            "session_follows" => self.follows.to_string(),
            "session_subs" => self.subs.to_string(),
            _ => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrocast_core::event::Origin;
    use ferrocast_core::sample::any_sample;

    #[test]
    fn session_tracks_labels() {
        let mut s = Session::default();
        for kind in ["follow", "subscription", "tip", "gift_batch"] {
            let mut e = any_sample(kind, Millis(0)).unwrap();
            e.origin = Origin::Live;
            s.record(&e);
        }
        assert_eq!(s.label("latest_follower"), "NovaRiver");
        assert_eq!(s.label("session_subs"), "6");
        assert_eq!(s.label("top_tip_session"), "Asha ₹500");
        assert_eq!(s.label("nonsense"), "");
    }

    #[test]
    fn why_is_bounded() {
        let mut st = State::default();
        for i in 0..(WHY_KEEP + 10) {
            st.remember_why(&i.to_string(), Why { alerted: true, reasons: vec![] });
        }
        assert_eq!(st.why.len(), WHY_KEEP);
        assert!(st.why("0").is_none());
        assert!(st.why(&(WHY_KEEP + 9).to_string()).is_some());
    }
}
