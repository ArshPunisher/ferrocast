//! Cross-platform moderation plans: panic mode and "ban everywhere" (R-50).
//!
//! Platforms differ in what an app may switch on. Planning is pure: given what each
//! connected platform supports, it lists exactly what will happen and what can't, so the
//! dashboard can say so plainly instead of pretending (R-09).

use serde::{Deserialize, Serialize};

use crate::event::{Actor, ChatMode, Millis, Platform};
use crate::filter::fold;

/// What the streamer wants when they hit the panic button (the dashboard's shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanicConfig {
    /// Only accounts that have followed for at least `followers_only_minutes` may chat.
    pub followers_only: bool,
    pub followers_only_minutes: u32,
    pub emote_only: bool,
    /// Slow mode delay; 0 = leave slow mode alone.
    pub slow_mode_seconds: u32,
    /// Hold first-time chatters' messages for a mod while panic is on.
    pub hold_new_accounts: bool,
    pub pause_alerts: bool,
    pub pause_tts: bool,
    /// The global shortcut, e.g. "Ctrl+Alt+Shift+P". Empty = none.
    #[serde(default)]
    pub hotkey: String,
    #[serde(default)]
    pub subscribers_only: bool,
}

impl Default for PanicConfig {
    fn default() -> Self {
        PanicConfig {
            followers_only: true,
            followers_only_minutes: 10,
            emote_only: false,
            slow_mode_seconds: 10,
            hold_new_accounts: true,
            pause_alerts: true,
            pause_tts: true,
            // Global, so it must not steal a common app shortcut (⌘⇧P, Ctrl+Shift+P).
            hotkey: "Ctrl+Alt+Shift+P".into(),
            subscribers_only: false,
        }
    }
}

impl PanicConfig {
    /// Checks limits every platform accepts; messages name the field.
    pub fn validate(&self) -> Result<(), String> {
        if self.slow_mode_seconds > 120 {
            return Err("slow_mode_seconds: slow mode is 1 to 120 seconds (0 leaves it alone).".into());
        }
        if self.followers_only_minutes > 129_600 {
            return Err("followers_only_minutes: followers-only can be up to 90 days.".into());
        }
        if self.hotkey.chars().count() > 40 || self.hotkey.chars().any(char::is_control) {
            return Err("hotkey: a shortcut like Ctrl+Shift+P, up to 40 characters.".into());
        }
        Ok(())
    }
}

/// One chat-mode change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeChange {
    pub mode: ChatMode,
    pub enabled: bool,
    /// Seconds for slow mode, minutes for followers-only.
    pub value: Option<u32>,
}

/// What a platform lets apps change, reported by its adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Capabilities {
    pub modes: Vec<ChatMode>,
    pub can_ban: bool,
    pub can_timeout: bool,
    pub can_delete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformPlan {
    pub platform: Platform,
    pub changes: Vec<ModeChange>,
    /// Modes the streamer asked for that this platform can't switch from an app.
    pub unsupported: Vec<ChatMode>,
}

/// The full panic plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanicPlan {
    pub platforms: Vec<PlatformPlan>,
    pub pause_alerts: bool,
    pub pause_tts: bool,
    pub hold_new_accounts: bool,
}

/// Plans panic mode `on` (or off, which reverses every mode the config would turn on).
pub fn plan_panic(cfg: &PanicConfig, connected: &[(Platform, Capabilities)], on: bool) -> PanicPlan {
    let wanted: Vec<ModeChange> = [
        cfg.followers_only.then_some(ModeChange {
            mode: ChatMode::FollowersOnly,
            enabled: on,
            value: Some(cfg.followers_only_minutes),
        }),
        cfg.emote_only.then_some(ModeChange { mode: ChatMode::EmoteOnly, enabled: on, value: None }),
        cfg.subscribers_only.then_some(ModeChange { mode: ChatMode::SubscribersOnly, enabled: on, value: None }),
        (cfg.slow_mode_seconds > 0).then_some(ModeChange {
            mode: ChatMode::Slow,
            enabled: on,
            value: Some(cfg.slow_mode_seconds),
        }),
    ]
    .into_iter()
    .flatten()
    .collect();
    let platforms = connected
        .iter()
        .map(|(platform, caps)| {
            let (changes, unsupported): (Vec<_>, Vec<_>) = wanted.iter().partition(|c| caps.modes.contains(&c.mode));
            PlatformPlan {
                platform: *platform,
                changes,
                unsupported: unsupported.into_iter().map(|c| c.mode).collect(),
            }
        })
        .collect();
    PanicPlan {
        platforms,
        pause_alerts: cfg.pause_alerts && on,
        pause_tts: cfg.pause_tts && on,
        hold_new_accounts: cfg.hold_new_accounts && on,
    }
}

/// One entry in the shared ban list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BanEntry {
    pub platform: Platform,
    pub user_id: Option<String>,
    pub login: String,
    pub reason: Option<String>,
    pub at: Millis,
    /// Other accounts of the same person the streamer linked by hand.
    #[serde(default)]
    pub linked: Vec<(Platform, String)>,
    #[serde(default)]
    pub display_name: String,
    /// Banned with "ban everywhere".
    #[serde(default)]
    pub everywhere: bool,
    /// Who banned them: a mod's name, or `None` for the streamer or an automatic rule.
    #[serde(default)]
    pub by: Option<String>,
    /// Banned from a phone while this PC was off (the cloud bot did it, Pro M10): the PC
    /// recorded it when it came back.
    #[serde(default)]
    pub offline: bool,
}

/// The shared ban list across platforms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BanList {
    pub entries: Vec<BanEntry>,
    /// Also ban someone with the same (folded) name on another platform. Off by default:
    /// "alex" on Twitch and "alex" on Kick are often different people.
    pub match_names: bool,
}

impl BanList {
    pub fn add(&mut self, entry: BanEntry) {
        let dup = self.entries.iter().any(|e| {
            e.platform == entry.platform
                && match (&e.user_id, &entry.user_id) {
                    (Some(a), Some(b)) => a == b,
                    _ => e.login.eq_ignore_ascii_case(&entry.login),
                }
        });
        if !dup {
            self.entries.push(entry);
        }
    }

    pub fn remove(&mut self, platform: Platform, user_id: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| !(e.platform == platform && e.user_id.as_deref() == Some(user_id)));
        before != self.entries.len()
    }

    /// The entry that says `actor` should be banned on sight, if any. Same-platform matches
    /// are by id (names can change); other platforms by a hand link, or by folded name when
    /// `match_names` is on.
    pub fn on_sight(&self, actor: &Actor) -> Option<&BanEntry> {
        let id = actor.id.as_deref();
        let name = fold(actor.login.as_deref().unwrap_or(&actor.display_name));
        self.entries.iter().find(|e| {
            if e.platform == actor.platform {
                return id.is_some() && e.user_id.as_deref() == id;
            }
            if e.linked.iter().any(|(p, uid)| *p == actor.platform && Some(uid.as_str()) == id) {
                return true;
            }
            self.match_names && !name.is_empty() && fold(&e.login) == name
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(modes: &[ChatMode]) -> Capabilities {
        Capabilities { modes: modes.to_vec(), can_ban: true, can_timeout: true, can_delete: true }
    }

    #[test]
    fn panic_plans_only_what_each_platform_supports() {
        let cfg = PanicConfig::default();
        let connected = vec![
            (
                Platform::Twitch,
                caps(&[ChatMode::FollowersOnly, ChatMode::Slow, ChatMode::EmoteOnly, ChatMode::SubscribersOnly]),
            ),
            (Platform::Youtube, caps(&[])),
        ];
        let plan = plan_panic(&cfg, &connected, true);
        assert_eq!(plan.platforms[0].changes.len(), 2);
        assert!(plan.platforms[0].unsupported.is_empty());
        assert!(plan.platforms[1].changes.is_empty());
        assert_eq!(plan.platforms[1].unsupported, vec![ChatMode::FollowersOnly, ChatMode::Slow]);
        assert!(plan.pause_alerts && plan.pause_tts);
    }

    #[test]
    fn panic_limits_and_the_dashboard_shape() {
        assert!(PanicConfig::default().validate().is_ok());
        let c = PanicConfig { slow_mode_seconds: 121, ..PanicConfig::default() };
        assert!(c.validate().unwrap_err().starts_with("slow_mode_seconds"));
        let c = PanicConfig { hotkey: "x".repeat(41), ..PanicConfig::default() };
        assert!(c.validate().is_err());
        let json = r#"{"followers_only":true,"followers_only_minutes":30,"emote_only":false,"slow_mode_seconds":0,
            "hold_new_accounts":true,"pause_alerts":true,"pause_tts":true,"hotkey":"Ctrl+Shift+P"}"#;
        let c: PanicConfig = serde_json::from_str(json).unwrap();
        let plan = plan_panic(&c, &[(Platform::Twitch, caps(&[ChatMode::FollowersOnly, ChatMode::Slow]))], true);
        assert_eq!(plan.platforms[0].changes.len(), 1, "slow mode 0 is left alone");
        assert!(plan.hold_new_accounts);
    }

    #[test]
    fn turning_panic_off_reverses_and_unpauses() {
        let plan = plan_panic(&PanicConfig::default(), &[(Platform::Kick, caps(&[ChatMode::Slow]))], false);
        assert_eq!(
            plan.platforms[0].changes,
            vec![ModeChange { mode: ChatMode::Slow, enabled: false, value: Some(10) }]
        );
        assert!(!plan.pause_alerts && !plan.pause_tts && !plan.hold_new_accounts);
    }

    fn actor(p: Platform, id: &str, login: &str) -> Actor {
        let mut a = Actor::named(p, login);
        a.id = Some(id.into());
        a
    }

    fn entry(p: Platform, id: &str, login: &str) -> BanEntry {
        BanEntry {
            platform: p,
            user_id: Some(id.into()),
            login: login.into(),
            reason: None,
            at: Millis(0),
            linked: vec![],
            display_name: login.into(),
            everywhere: false,
            by: None,
            offline: false,
        }
    }

    #[test]
    fn on_sight_by_id_link_and_opt_in_name() {
        let mut list = BanList::default();
        let mut e = entry(Platform::Twitch, "t1", "Raider99");
        e.linked.push((Platform::Kick, "k7".into()));
        list.add(e);
        assert!(list.on_sight(&actor(Platform::Twitch, "t1", "renamed")).is_some(), "same id, new name");
        assert!(list.on_sight(&actor(Platform::Twitch, "t2", "raider99")).is_none(), "same name, other id");
        assert!(list.on_sight(&actor(Platform::Kick, "k7", "x")).is_some(), "linked account");
        assert!(list.on_sight(&actor(Platform::Youtube, "y", "raider99")).is_none(), "names off by default");
        list.match_names = true;
        assert!(list.on_sight(&actor(Platform::Youtube, "y", "rаider99")).is_some(), "folded name with a Cyrillic а");
    }

    #[test]
    fn add_dedupes_and_remove_works() {
        let mut list = BanList::default();
        list.add(entry(Platform::Twitch, "1", "a"));
        list.add(entry(Platform::Twitch, "1", "a-renamed"));
        assert_eq!(list.entries.len(), 1);
        assert!(list.remove(Platform::Twitch, "1"));
        assert!(!list.remove(Platform::Twitch, "1"));
    }
}
