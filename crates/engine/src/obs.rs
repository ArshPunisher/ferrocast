//! OBS settings and "Add to OBS" (VISION: few clicks).
//!
//! The host and port are settings; the password lives in the keychain (R-45). Each call
//! connects fresh with a short timeout: OBS is often closed and reopened while Ferrocast
//! runs, and a stale socket would make the button lie.

use ferrocast_obs::{Added, ObsConfig, ObsError, ObsStatus};
use ferrocast_store::repo;
use serde::Serialize;

use crate::state::ConnectorView;
use crate::{Engine, EngineError};

const PASSWORD_KEY: &str = "obs-password";

/// OBS as the dashboard sees it.
#[derive(Debug, Clone, Serialize)]
pub struct ObsView {
    pub state: String,
    pub host: String,
    pub port: u16,
    pub has_password: bool,
    pub detail: Option<String>,
    pub status: Option<ObsStatus>,
}

/// What auto-detect found in OBS's own settings. The password itself never leaves the
/// engine: "Use these" saves it straight to the keychain.
#[derive(Debug, Clone, Serialize)]
pub struct ObsDetectView {
    pub found: bool,
    pub port: Option<u16>,
    pub has_password: bool,
    /// OBS's WebSocket server is switched on.
    pub server_enabled: bool,
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

impl Engine {
    async fn obs_config(&self) -> Result<ObsConfig, EngineError> {
        self.db(|s| repo::settings::get_or_default::<ObsConfig>(s, "obs")).await
    }

    fn obs_password(&self) -> Result<Option<String>, EngineError> {
        self.inner.secrets.get(PASSWORD_KEY).map_err(|e| EngineError::Unavailable(e.to_string()))
    }

    fn set_obs_state(&self, state: &str, detail: Option<String>) {
        let mut st = self.state_mut();
        let v = st.connectors.entry("obs".into()).or_insert_with(ConnectorView::off);
        v.state = state.into();
        v.detail = detail;
        v.last_event_at = Some(crate::now().0);
    }

    /// Tries OBS now and reports what happened.
    pub async fn obs(&self) -> Result<ObsView, EngineError> {
        let cfg = self.obs_config().await?;
        let password = self.obs_password()?;
        let mut view = ObsView {
            state: "disconnected".into(),
            host: cfg.host.clone(),
            port: cfg.port,
            has_password: password.is_some(),
            detail: None,
            status: None,
        };
        match ferrocast_obs::connect(&cfg, password.as_deref()).await {
            Ok(client) => match ferrocast_obs::status(&client).await {
                Ok(s) => {
                    view.state = "connected".into();
                    view.status = Some(s);
                }
                Err(e) => view.detail = Some(e.to_string()),
            },
            Err(e) => view.detail = Some(e.to_string()),
        }
        self.set_obs_state(&view.state, view.detail.clone());
        Ok(view)
    }

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
            let saved = ObsConfig { host: host.to_string(), port: cfg.port };
            self.db(move |s| repo::settings::set(s, "obs", &saved)).await?;
            let r = match password {
                PasswordChange::Keep => Ok(()),
                PasswordChange::Clear => self.inner.secrets.delete(PASSWORD_KEY),
                PasswordChange::Set(p) => self.inner.secrets.set(PASSWORD_KEY, &p),
            };
            r.map_err(|e| EngineError::Unavailable(e.to_string()))?;
        }
        self.obs().await
    }

    /// Looks for OBS's WebSocket settings on this computer.
    pub async fn obs_detect(&self) -> Result<ObsDetectView, EngineError> {
        self.obs_detect_in(ferrocast_obs::detect::default_paths()).await.map(|(v, _)| v)
    }

    /// [`Engine::obs_detect`] in chosen places (tests), with the details kept inside.
    pub async fn obs_detect_in(
        &self,
        paths: Vec<std::path::PathBuf>,
    ) -> Result<(ObsDetectView, Option<ferrocast_obs::detect::Detected>), EngineError> {
        use ferrocast_obs::detect::{Detection, detect};
        let found = tokio::task::spawn_blocking(move || detect(&paths))
            .await
            .map_err(|e| EngineError::Internal(e.to_string()))?;
        Ok(match found {
            Detection::Found(d) => (
                ObsDetectView {
                    found: true,
                    port: Some(d.port),
                    has_password: d.password.is_some(),
                    server_enabled: d.server_enabled,
                    reason: (!d.server_enabled).then(|| {
                        "OBS's WebSocket server is switched off. In OBS open Tools, WebSocket Server Settings and tick Enable WebSocket server.".to_string()
                    }),
                },
                Some(d),
            ),
            Detection::Missing => (
                ObsDetectView {
                    found: false,
                    port: None,
                    has_password: false,
                    server_enabled: false,
                    reason: Some("OBS's settings aren't on this computer. Is OBS 28 or newer installed here?".into()),
                },
                None,
            ),
            Detection::Unreadable(why) => (
                ObsDetectView { found: false, port: None, has_password: false, server_enabled: false, reason: Some(why) },
                None,
            ),
        })
    }

    /// Uses what was detected: OBS on this computer, its port, and its password (to the
    /// keychain). 422 with the reason when nothing usable was found.
    pub async fn obs_use_detected(&self) -> Result<ObsView, EngineError> {
        self.obs_use_detected_in(ferrocast_obs::detect::default_paths()).await
    }

    pub async fn obs_use_detected_in(&self, paths: Vec<std::path::PathBuf>) -> Result<ObsView, EngineError> {
        let (view, found) = self.obs_detect_in(paths).await?;
        let Some(d) = found else {
            return Err(EngineError::Unavailable(
                view.reason.unwrap_or_else(|| "OBS's settings weren't found.".into()),
            ));
        };
        let change = match d.password {
            Some(p) => PasswordChange::Set(p),
            None => PasswordChange::Clear,
        };
        self.save_obs(ObsConfig { host: "127.0.0.1".into(), port: d.port }, change).await
    }

    /// Puts an overlay into OBS's current scene, or fixes the existing source.
    pub async fn add_to_obs(&self, overlay_id: &str) -> Result<Added, EngineError> {
        let o = self.overlay(overlay_id).await?;
        let view = self.overlay_view(&o);
        let cfg = self.obs_config().await?;
        let password = self.obs_password()?;
        let client = match ferrocast_obs::connect(&cfg, password.as_deref()).await {
            Ok(c) => c,
            Err(e) => {
                self.set_obs_state("disconnected", Some(e.to_string()));
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
