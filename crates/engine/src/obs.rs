//! OBS: connection settings, the background connection, and "Add to OBS" (VISION: few clicks).
//!
//! **Which details are used.** OBS keeps its WebSocket port and password in its own settings
//! file, which [`ferrocast_obs::detect`] reads. Until the streamer types OBS details
//! themselves, Ferrocast follows that file ("auto"): every connection attempt re-reads it,
//! so switching the WebSocket server on in OBS (or changing its password) connects within
//! seconds, without a click. The detected password is read from OBS's file each time and
//! is never copied into Ferrocast's secret store.
//!
//! The details count as **the streamer's own** ("manual") once they save the OBS form
//! (PUT `/obs`) with anything different from what's in use. From then on Ferrocast uses
//! exactly what they typed and never replaces it with detected values; "Use these"
//! (POST `/obs/detect`) switches back to following OBS. Settings saved before this rule
//! existed count as manual only when the address or port differ from the defaults
//! (`127.0.0.1:4455`): a local OBS with a typed password is exactly what auto-detect finds.
//!
//! A typed password lives in the secret store (R-45), never in settings. A background task
//! ([`Engine::watch_obs`]) keeps one connection open, checks it every few seconds, and
//! retries while OBS is closed; every change of state reaches dashboards as one `status`
//! frame, so the chip never says "Disconnected" while OBS works (or the other way round).

use std::path::PathBuf;
use std::time::Duration;

use ferrocast_obs::detect::{Detected, Detection};
use ferrocast_obs::{Added, ObsConfig, ObsError, ObsStatus};
use ferrocast_store::repo;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::frames::LiveFrame;
use crate::state::ConnectorView;
use crate::{Engine, EngineError};

const PASSWORD_KEY: &str = "obs-password";
const SETTINGS_KEY: &str = "obs";
const LOCAL_HOST: &str = "127.0.0.1";
/// How often a working connection is checked.
const CHECK_CONNECTED: Duration = Duration::from_secs(5);
/// How often a closed OBS is tried again.
const RETRY: Duration = Duration::from_secs(3);

/// OBS as the dashboard sees it.
#[derive(Debug, Clone, Serialize)]
pub struct ObsView {
    pub state: String,
    pub host: String,
    pub port: u16,
    pub has_password: bool,
    /// Following OBS's own settings (nothing typed by the streamer).
    pub auto: bool,
    pub detail: Option<String>,
    pub status: Option<ObsStatus>,
}

/// What auto-detect found in OBS's own settings. The password itself never leaves the
/// engine.
#[derive(Debug, Clone, Serialize)]
pub struct ObsDetectView {
    pub found: bool,
    pub port: Option<u16>,
    pub has_password: bool,
    /// OBS's WebSocket server is switched on.
    pub server_enabled: bool,
    /// Ferrocast already uses these details (following OBS, or the typed ones match), so
    /// there's nothing to offer.
    pub in_use: bool,
    /// Why nothing usable was found, in plain words.
    pub reason: Option<String>,
}

/// What to do with the stored password when saving OBS settings.
#[derive(Debug, Clone)]
pub enum PasswordChange {
    Keep,
    Clear,
    Set(String),
}

impl From<ObsError> for EngineError {
    fn from(e: ObsError) -> Self {
        EngineError::Unavailable(e.to_string())
    }
}

/// The saved OBS settings (`settings` key `obs`). Older saves have no `manual` flag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ObsSettings {
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual: Option<bool>,
}

impl Default for ObsSettings {
    fn default() -> Self {
        let d = ObsConfig::default();
        ObsSettings { host: d.host, port: d.port, manual: None }
    }
}

impl ObsSettings {
    /// Whether the streamer typed these details themselves (see the module docs).
    pub fn is_manual(&self) -> bool {
        self.manual.unwrap_or_else(|| {
            let d = ObsConfig::default();
            self.host != d.host || self.port != d.port
        })
    }
}

/// Where and how to connect, decided fresh for every attempt.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub cfg: ObsConfig,
    pub password: Option<String>,
    pub auto: bool,
    /// Auto, and OBS's settings say its WebSocket server is off: the reason to show.
    pub switched_off: bool,
    /// Auto, and OBS isn't installed here (nothing was ever set up).
    pub nothing_found: bool,
}

impl std::fmt::Debug for Target {
    // Never print the password.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Target({}:{}, auto={}, password={})",
            self.cfg.host,
            self.cfg.port,
            self.auto,
            self.password.is_some()
        )
    }
}

/// The rule, without I/O: the streamer's own details win; otherwise OBS's file, when found.
pub(crate) fn resolve(saved: &ObsSettings, saved_password: Option<String>, detection: &Detection) -> Target {
    let typed = || ObsConfig { host: saved.host.clone(), port: saved.port };
    if saved.is_manual() {
        return Target {
            cfg: typed(),
            password: saved_password,
            auto: false,
            switched_off: false,
            nothing_found: false,
        };
    }
    match detection {
        Detection::Found(d) => Target {
            cfg: ObsConfig { host: LOCAL_HOST.into(), port: d.port },
            password: d.password.clone(),
            auto: true,
            switched_off: !d.server_enabled,
            nothing_found: false,
        },
        // Nothing (usable) found: still try what's saved, OBS may be a portable install.
        Detection::Missing | Detection::Unreadable(_) => Target {
            cfg: typed(),
            password: saved_password,
            auto: true,
            switched_off: false,
            nothing_found: matches!(detection, Detection::Missing),
        },
    }
}

const SWITCHED_OFF: &str = "OBS's WebSocket server is switched off. In OBS open Tools, WebSocket Server Settings and tick Enable WebSocket server.";

/// Waits for the next check: 5 s while connected, 3 s while not, or at once when the
/// settings change (then the connection is opened again with the new details).
async fn continue_wait(engine: &Engine, open: &mut Option<(ferrocast_obs::Client, Target)>) {
    let wait = if open.is_some() { CHECK_CONNECTED } else { RETRY };
    tokio::select! {
        () = tokio::time::sleep(wait) => {}
        () = engine.inner.obs_wake.notified() => { *open = None; }
    }
}

impl Engine {
    async fn obs_settings(&self) -> Result<ObsSettings, EngineError> {
        self.db(|s| repo::settings::get_or_default::<ObsSettings>(s, SETTINGS_KEY)).await
    }

    fn obs_password(&self) -> Result<Option<String>, EngineError> {
        self.inner.secrets.get(PASSWORD_KEY).map_err(|e| EngineError::Unavailable(e.to_string()))
    }

    /// Where OBS's settings are looked for (tests point this at a temp folder).
    pub fn set_obs_detect_paths(&self, paths: Vec<PathBuf>) {
        *self.inner.obs_paths.write().unwrap_or_else(|p| p.into_inner()) = paths;
        self.inner.obs_wake.notify_one();
    }

    fn obs_paths(&self) -> Vec<PathBuf> {
        self.inner.obs_paths.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    async fn detect_obs_in(paths: Vec<PathBuf>) -> Result<Detection, EngineError> {
        tokio::task::spawn_blocking(move || ferrocast_obs::detect::detect(&paths))
            .await
            .map_err(|e| EngineError::Internal(e.to_string()))
    }

    /// What to connect to right now (re-reads OBS's settings file when following it).
    pub(crate) async fn obs_target(&self) -> Result<Target, EngineError> {
        let saved = self.obs_settings().await?;
        let password = self.obs_password()?;
        let detection =
            if saved.is_manual() { Detection::Missing } else { Self::detect_obs_in(self.obs_paths()).await? };
        Ok(resolve(&saved, password, &detection))
    }

    /// Records OBS's state; dashboards get one `status` frame when it changes.
    fn set_obs_state(&self, state: &str, detail: Option<String>) {
        let changed = {
            let mut st = self.state_mut();
            let v = st.connectors.entry("obs".into()).or_insert_with(ConnectorView::off);
            let changed = v.state != state || v.detail != detail;
            if changed {
                v.state = state.into();
                v.detail = detail;
                v.last_event_at = Some(crate::now().0);
            }
            changed.then(|| st.connectors.clone())
        };
        if let Some(view) = changed {
            self.send_live(LiveFrame::new("status", &json!({ "connectors": view })));
        }
    }

    /// The state and detail for a failed attempt.
    fn obs_failure(target: &Target, e: &ObsError) -> (&'static str, String) {
        if target.switched_off {
            ("disconnected", SWITCHED_OFF.into())
        } else if target.nothing_found && *e == ObsError::NotReachable {
            ("off", "OBS wasn't found on this computer. Open OBS here, or enter its address below.".into())
        } else {
            ("disconnected", e.to_string())
        }
    }

    /// One attempt with the current details: connect, ask OBS how it is, record the state.
    async fn obs_attempt(&self, target: &Target) -> (Option<ferrocast_obs::Client>, Option<ObsStatus>, Option<String>) {
        match ferrocast_obs::connect(&target.cfg, target.password.as_deref()).await {
            Ok(client) => match ferrocast_obs::status(&client).await {
                Ok(s) => {
                    self.set_obs_state("connected", None);
                    (Some(client), Some(s), None)
                }
                Err(e) => {
                    let (state, detail) = Self::obs_failure(target, &e);
                    self.set_obs_state(state, Some(detail.clone()));
                    (None, None, Some(detail))
                }
            },
            Err(e) => {
                let (state, detail) = Self::obs_failure(target, &e);
                self.set_obs_state(state, Some(detail.clone()));
                (None, None, Some(detail))
            }
        }
    }

    /// Starts the background OBS connection (desktop and headless; tests start it when
    /// they want it). It connects with the details [`Engine::obs_target`] picks, checks a
    /// working connection every 5 s, retries a closed OBS every 3 s, and starts over at
    /// once when the settings change.
    pub fn watch_obs(&self) {
        let engine = self.clone();
        tokio::spawn(async move {
            let mut open: Option<(ferrocast_obs::Client, Target)> = None;
            let mut last_error: Option<String> = None;
            loop {
                match engine.obs_target().await {
                    Ok(target) => {
                        last_error = None;
                        let still_good = match &open {
                            Some((client, used)) if *used == target => ferrocast_obs::status(client).await.is_ok(),
                            _ => false,
                        };
                        if !still_good {
                            open = None;
                            if let (Some(client), _, _) = engine.obs_attempt(&target).await {
                                open = Some((client, target));
                            }
                        }
                    }
                    Err(e) => {
                        // Once per change, not every few seconds; the dashboard shows why.
                        let why = e.to_string();
                        if last_error.as_deref() != Some(why.as_str()) {
                            tracing::warn!(error = %why, "OBS settings couldn't be read");
                        }
                        engine.set_obs_state("disconnected", Some(why.clone()));
                        last_error = Some(why);
                        open = None;
                    }
                }
                continue_wait(&engine, &mut open).await;
            }
        });
    }

    /// Tries OBS now and reports what happened.
    pub async fn obs(&self) -> Result<ObsView, EngineError> {
        let target = self.obs_target().await?;
        let (_, status, detail) = self.obs_attempt(&target).await;
        let state = self.state().connectors.get("obs").map(|c| c.state.clone()).unwrap_or_else(|| "off".into());
        Ok(ObsView {
            state,
            host: target.cfg.host.clone(),
            port: target.cfg.port,
            has_password: target.password.is_some(),
            auto: target.auto,
            detail,
            status,
        })
    }

    /// Saves what the streamer typed. Saving the details already in use (say, pressing
    /// Connect without changing anything while Ferrocast follows OBS) changes nothing.
    pub async fn save_obs(&self, cfg: ObsConfig, password: PasswordChange) -> Result<ObsView, EngineError> {
        let host = cfg.host.trim();
        if host.is_empty() || host.len() > 253 || host.contains(['/', ' ', '@']) {
            return Err(EngineError::Invalid("Enter OBS's address, like 127.0.0.1 or my-pc.local.".into()));
        }
        if cfg.port == 0 {
            return Err(EngineError::Invalid("The port must be 1 to 65535 (OBS uses 4455).".into()));
        }
        {
            let _one_at_a_time = self.config_lock().await;
            let current = self.obs_target().await?;
            let unchanged =
                matches!(password, PasswordChange::Keep) && current.cfg.host == host && current.cfg.port == cfg.port;
            if !(unchanged && current.auto) {
                // The password first: if it can't be saved, nothing changes. Leaving "follow
                // OBS" with the password field blank keeps the one in use (read from OBS's
                // file), so changing only the address or port doesn't lose it.
                let r = match password {
                    PasswordChange::Keep if current.auto => match &current.password {
                        Some(p) => self.inner.secrets.set(PASSWORD_KEY, p),
                        None => Ok(()),
                    },
                    PasswordChange::Keep => Ok(()),
                    PasswordChange::Clear => self.inner.secrets.delete(PASSWORD_KEY),
                    PasswordChange::Set(p) => self.inner.secrets.set(PASSWORD_KEY, &p),
                };
                r.map_err(|e| EngineError::Unavailable(e.to_string()))?;
                let saved = ObsSettings { host: host.to_string(), port: cfg.port, manual: Some(true) };
                self.db(move |s| repo::settings::set(s, SETTINGS_KEY, &saved)).await?;
            }
        }
        self.inner.obs_wake.notify_one();
        self.obs().await
    }

    /// Looks for OBS's WebSocket settings on this computer.
    pub async fn obs_detect(&self) -> Result<ObsDetectView, EngineError> {
        self.obs_detect_in(self.obs_paths()).await.map(|(v, _)| v)
    }

    /// [`Engine::obs_detect`] in chosen places (tests), with the details kept inside.
    pub async fn obs_detect_in(&self, paths: Vec<PathBuf>) -> Result<(ObsDetectView, Option<Detected>), EngineError> {
        let found = Self::detect_obs_in(paths).await?;
        Ok(match found {
            Detection::Found(d) => {
                let saved = self.obs_settings().await?;
                let in_use = !saved.is_manual() || {
                    let typed = resolve(&saved, self.obs_password()?, &Detection::Missing);
                    let local = matches!(typed.cfg.host.as_str(), LOCAL_HOST | "localhost" | "::1");
                    local && typed.cfg.port == d.port && typed.password == d.password
                };
                (
                    ObsDetectView {
                        found: true,
                        port: Some(d.port),
                        has_password: d.password.is_some(),
                        server_enabled: d.server_enabled,
                        in_use,
                        reason: (!d.server_enabled).then(|| SWITCHED_OFF.to_string()),
                    },
                    Some(d),
                )
            }
            Detection::Missing => (
                ObsDetectView {
                    found: false,
                    port: None,
                    has_password: false,
                    server_enabled: false,
                    in_use: false,
                    reason: Some("OBS's settings aren't on this computer. Is OBS 28 or newer installed here?".into()),
                },
                None,
            ),
            Detection::Unreadable(why) => (
                ObsDetectView {
                    found: false,
                    port: None,
                    has_password: false,
                    server_enabled: false,
                    in_use: false,
                    reason: Some(why),
                },
                None,
            ),
        })
    }

    /// Goes back to following OBS's own settings (and forgets a typed password). 422 with
    /// the reason when nothing usable was found.
    pub async fn obs_use_detected(&self) -> Result<ObsView, EngineError> {
        self.obs_use_detected_in(self.obs_paths()).await
    }

    pub async fn obs_use_detected_in(&self, paths: Vec<PathBuf>) -> Result<ObsView, EngineError> {
        let (view, found) = self.obs_detect_in(paths.clone()).await?;
        if found.is_none() {
            return Err(EngineError::Unavailable(
                view.reason.unwrap_or_else(|| "OBS's settings weren't found.".into()),
            ));
        }
        {
            let _one_at_a_time = self.config_lock().await;
            let saved = ObsSettings { manual: Some(false), ..ObsSettings::default() };
            self.db(move |s| repo::settings::set(s, SETTINGS_KEY, &saved)).await?;
            self.inner.secrets.delete(PASSWORD_KEY).map_err(|e| EngineError::Unavailable(e.to_string()))?;
        }
        if paths != self.obs_paths() {
            self.set_obs_detect_paths(paths);
        }
        self.inner.obs_wake.notify_one();
        self.obs().await
    }

    /// Puts an overlay into OBS's current scene, or fixes the existing source.
    pub async fn add_to_obs(&self, overlay_id: &str) -> Result<Added, EngineError> {
        let o = self.overlay(overlay_id).await?;
        let view = self.overlay_view(&o);
        let target = self.obs_target().await?;
        let client = match ferrocast_obs::connect(&target.cfg, target.password.as_deref()).await {
            Ok(c) => c,
            Err(e) => {
                let (state, detail) = Self::obs_failure(&target, &e);
                self.set_obs_state(state, Some(detail));
                return Err(e.into());
            }
        };
        self.set_obs_state("connected", None);
        let added = ferrocast_obs::add_browser_source(
            &client,
            &ferrocast_obs::source_name(&o.name),
            &view.url,
            u32::try_from(o.width).unwrap_or(1920),
            u32::try_from(o.height).unwrap_or(1080),
        )
        .await?;
        Ok(added)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(port: u16, password: Option<&str>, enabled: bool) -> Detection {
        Detection::Found(Detected {
            path: PathBuf::from("/x/config.json"),
            port,
            server_enabled: enabled,
            auth_required: password.is_some(),
            password: password.map(str::to_string),
        })
    }

    #[test]
    fn untouched_settings_follow_obs() {
        let t = resolve(&ObsSettings::default(), None, &found(4466, Some("from-obs"), true));
        assert_eq!(
            (t.cfg.host.as_str(), t.cfg.port, t.password.as_deref(), t.auto),
            (LOCAL_HOST, 4466, Some("from-obs"), true)
        );
        assert!(!t.switched_off && !t.nothing_found);
        let off = resolve(&ObsSettings::default(), None, &found(4455, None, false));
        assert!(off.auto && off.switched_off);
        let none = resolve(&ObsSettings::default(), None, &Detection::Missing);
        assert!(none.auto && none.nothing_found && none.cfg == ObsConfig::default());
        assert!(!format!("{t:?}").contains("from-obs"), "Debug never shows the password");
    }

    #[test]
    fn typed_details_always_win() {
        let typed = ObsSettings { host: "studio-pc.local".into(), port: 4460, manual: Some(true) };
        let t = resolve(&typed, Some("typed".into()), &found(4466, Some("from-obs"), true));
        assert_eq!(
            (t.cfg.host.as_str(), t.cfg.port, t.password.as_deref(), t.auto),
            ("studio-pc.local", 4460, Some("typed"), false)
        );
        // Even typed defaults stay the streamer's.
        let typed = ObsSettings { manual: Some(true), ..ObsSettings::default() };
        assert!(!resolve(&typed, None, &found(4466, Some("p"), true)).auto);
        // "Use these" sets manual = false, whatever the address.
        let back = ObsSettings { manual: Some(false), ..ObsSettings::default() };
        assert!(resolve(&back, None, &found(4466, None, true)).auto);
    }

    #[test]
    fn old_saves_count_as_typed_only_when_they_differ_from_the_defaults() {
        let parse = |s: &str| serde_json::from_str::<ObsSettings>(s).unwrap();
        assert!(!parse(r#"{"host":"127.0.0.1","port":4455}"#).is_manual());
        assert!(parse(r#"{"host":"192.168.1.20","port":4455}"#).is_manual());
        assert!(parse(r#"{"host":"127.0.0.1","port":4460}"#).is_manual());
        assert!(!parse(r#"{"host":"10.0.0.2","port":4460,"manual":false}"#).is_manual());
        // Old readers of the setting still understand it.
        let cfg: ObsConfig = serde_json::from_str(r#"{"host":"127.0.0.1","port":4455,"manual":true}"#).unwrap();
        assert_eq!(cfg, ObsConfig::default());
    }
}
