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
    /// The dashboard's look: "system", "dark" or "light".
    pub theme: String,
    /// "system", "on" or "off".
    pub reduced_motion: String,
    /// Lighter overlay effects for slower PCs (R-40).
    pub low_motion_overlays: bool,
    /// Let phones and tablets on this network open the dashboard (applies on next start).
    pub lan_access: bool,
    /// "stable" or "beta".
    pub update_channel: String,
    /// Never install an update while any platform is live (R-60).
    pub never_update_while_live: bool,
    pub launch_at_login: bool,
    pub close_to_tray: bool,
    /// A BCP 47 tag like "en" or "hi".
    pub language: String,
    /// The streamer's main currency (ISO 4217), e.g. for the TTS minimum. Defaults from the
    /// computer's region.
    pub currency: String,
}

impl Default for EngineSettings {
    fn default() -> Self {
        EngineSettings {
            keep_chat_days: 30,
            test_alerts: true,
            bot_enabled: true,
            auto_ban_on_sight: true,
            theme: "system".into(),
            reduced_motion: "system".into(),
            low_motion_overlays: false,
            lan_access: false,
            update_channel: "stable".into(),
            never_update_while_live: true,
            launch_at_login: false,
            close_to_tray: true,
            language: "en".into(),
            currency: currency_for_locale(sys_locale::get_locale().as_deref().unwrap_or("")).into(),
        }
    }
}

/// A sensible main currency from a locale like "en-IN" or "pt_BR.UTF-8".
pub fn currency_for_locale(locale: &str) -> &'static str {
    let region = locale
        .split(['.', '@'])
        .next()
        .unwrap_or("")
        .split(['-', '_'])
        .skip(1)
        .find(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_alphabetic()))
        .map(|p| p.to_ascii_uppercase())
        .unwrap_or_default();
    const EURO: [&str; 20] = [
        "AT", "BE", "CY", "DE", "EE", "ES", "FI", "FR", "GR", "HR", "IE", "IT", "LT", "LU", "LV", "MT", "NL", "PT",
        "SI", "SK",
    ];
    if EURO.contains(&region.as_str()) {
        return "EUR";
    }
    match region.as_str() {
        "IN" => "INR",
        "GB" => "GBP",
        "BR" => "BRL",
        "CA" => "CAD",
        "AU" => "AUD",
        "NZ" => "NZD",
        "JP" => "JPY",
        "KR" => "KRW",
        "MX" => "MXN",
        "ID" => "IDR",
        "PH" => "PHP",
        "TR" => "TRY",
        "PL" => "PLN",
        "SE" => "SEK",
        "NO" => "NOK",
        "DK" => "DKK",
        "CH" => "CHF",
        "SG" => "SGD",
        "AE" => "AED",
        "ZA" => "ZAR",
        "PK" => "PKR",
        "BD" => "BDT",
        _ => "USD",
    }
}

impl EngineSettings {
    /// Checks every field; the message names the field.
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=3650).contains(&self.keep_chat_days) {
            return Err("keep_chat_days: keep chat for 1 to 3650 days.".into());
        }
        let one_of = |field: &str, v: &str, ok: &[&str]| {
            if ok.contains(&v) { Ok(()) } else { Err(format!("{field}: use one of {}.", ok.join(", "))) }
        };
        one_of("theme", &self.theme, &["system", "dark", "light"])?;
        one_of("reduced_motion", &self.reduced_motion, &["system", "on", "off"])?;
        one_of("update_channel", &self.update_channel, &["stable", "beta"])?;
        if self.currency.len() != 3 || !self.currency.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err("currency: a three-letter code like INR or USD.".into());
        }
        let lang_ok = (2..=35).contains(&self.language.len())
            && self.language.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if !lang_ok {
            return Err("language: a language tag like en or hi-IN.".into());
        }
        Ok(())
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
    pub loyalty: ferrocast_core::loyalty::LoyaltyConfig,
    pub welcome: ferrocast_core::welcome::WelcomeConfig,
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
    /// Money this stream, one entry per currency (never converted).
    #[serde(default)]
    pub tips: Vec<Money>,
    /// Bits cheered this stream.
    #[serde(default)]
    pub bits: u64,
    /// The most people watching at once, all platforms together.
    #[serde(default)]
    pub peak_viewers: u64,
    /// Payments already taken off `tips` (a refund and a dispute can both arrive).
    #[serde(skip)]
    pub reversed: std::collections::HashSet<String>,
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
                    match self.tips.iter_mut().find(|m| m.currency == money.currency) {
                        Some(m) => m.minor = m.minor.saturating_add(money.minor),
                        None => self.tips.push(money.clone()),
                    }
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
            EventKind::PlatformGift { value, .. } if value.unit == ferrocast_core::money::ValueUnit::Bits => {
                self.bits = self.bits.saturating_add(value.amount);
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

#[cfg(test)]
mod locale_tests {
    use super::currency_for_locale;

    #[test]
    fn regions_pick_their_currency() {
        assert_eq!(currency_for_locale("en-IN"), "INR");
        assert_eq!(currency_for_locale("hi_IN.UTF-8"), "INR");
        assert_eq!(currency_for_locale("pt-BR"), "BRL");
        assert_eq!(currency_for_locale("de_DE@euro"), "EUR");
        assert_eq!(currency_for_locale("zh-Hans-SG"), "SGD");
        assert_eq!(currency_for_locale("en"), "USD");
        assert_eq!(currency_for_locale(""), "USD");
    }
}
