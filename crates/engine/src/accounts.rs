//! Signing platforms in and out, and running their adapters.
//!
//! Tokens live in the keychain (R-45); the `accounts` table only remembers who is
//! connected. On start, every saved account's adapter starts by itself.

use std::sync::Arc;
use std::time::Duration;

use ferrocast_core::event::{ConnectorStatus, Event, EventKind, Millis, Platform};
use ferrocast_store::repo;
use ferrocast_twitch::auth::{self as twitch_auth, Poll, Tokens};
use ferrocast_twitch::{Auth, Endpoints, TwitchConnector};
use serde::Serialize;

use crate::frames::{LiveFrame, Notice, NoticeLevel};
use crate::{Engine, EngineError, now};

const TWITCH_TOKENS: &str = "twitch-tokens";

/// The public client ids of Ferrocast's platform apps. They're public by design (device
/// code / PKCE flows); release builds bake them in, and `FERROCAST_TWITCH_CLIENT_ID` etc.
/// override them for development (R-46: never a secret in the app).
#[derive(Debug, Clone, Default)]
pub struct PlatformApps {
    pub twitch_client_id: Option<String>,
    pub twitch_endpoints: Endpoints,
}

impl PlatformApps {
    pub fn from_env() -> Self {
        let pick = |runtime: &str, built: Option<&'static str>| {
            std::env::var(runtime)
                .ok()
                .filter(|s| !s.is_empty())
                .or_else(|| built.map(String::from))
                .filter(|s| !s.is_empty())
        };
        PlatformApps {
            twitch_client_id: pick("FERROCAST_TWITCH_CLIENT_ID", option_env!("FERROCAST_TWITCH_CLIENT_ID")),
            twitch_endpoints: Endpoints::default(),
        }
    }
}

/// What the dashboard shows while the streamer signs in.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "flow", rename_all = "snake_case")]
pub enum ConnectFlow {
    DeviceCode { user_code: String, verification_url: String, expires_at: i64 },
}

impl Engine {
    pub(crate) fn apps(&self) -> &PlatformApps {
        &self.inner.apps
    }

    /// Starts signing in to `platform`. Finishing happens in the background; the dashboard
    /// watches the `status` frames.
    pub async fn connect_account(&self, platform: Platform) -> Result<ConnectFlow, EngineError> {
        match platform {
            Platform::Twitch => self.connect_twitch().await,
            Platform::Youtube | Platform::Kick => {
                Err(EngineError::Unavailable(format!("Signing in to {platform} isn't available in this build yet.")))
            }
            _ => Err(EngineError::Invalid("That isn't a platform you can sign in to.".into())),
        }
    }

    async fn connect_twitch(&self) -> Result<ConnectFlow, EngineError> {
        let client_id = self.apps().twitch_client_id.clone().ok_or_else(|| {
            EngineError::Unavailable(
                "This build of Ferrocast has no Twitch app set up, so it can't sign in to Twitch.".into(),
            )
        })?;
        let ep = self.apps().twitch_endpoints.clone();
        let http = ferrocast_twitch::http_client();
        let code =
            twitch_auth::start(&http, &ep, &client_id).await.map_err(|e| EngineError::Unavailable(e.to_string()))?;
        let flow = ConnectFlow::DeviceCode {
            user_code: code.user_code.clone(),
            verification_url: code.verification_uri.clone(),
            expires_at: now().0 + i64::try_from(code.expires_in).unwrap_or(1800) * 1000,
        };
        let engine = self.clone();
        tokio::spawn(async move {
            let mut interval = Duration::from_secs(code.interval.clamp(1, 30));
            let deadline = tokio::time::Instant::now() + Duration::from_secs(code.expires_in.min(1800));
            loop {
                tokio::time::sleep(interval).await;
                if tokio::time::Instant::now() > deadline {
                    engine.notice(NoticeLevel::Warn, "The Twitch sign-in code expired. Press Connect again.");
                    return;
                }
                match twitch_auth::poll(&http, &ep, &client_id, &code.device_code).await {
                    Ok(Poll::Pending) => {}
                    Ok(Poll::SlowDown) => interval += Duration::from_secs(5),
                    Ok(Poll::Done(tokens)) => {
                        if let Err(e) = engine.finish_twitch(tokens).await {
                            engine.notice(NoticeLevel::Error, &format!("Twitch sign-in didn't finish: {e}"));
                        }
                        return;
                    }
                    Err(e) => {
                        engine.notice(NoticeLevel::Warn, &e.to_string());
                        return;
                    }
                }
            }
        });
        Ok(flow)
    }

    async fn finish_twitch(&self, tokens: Tokens) -> Result<(), EngineError> {
        let ep = self.apps().twitch_endpoints.clone();
        let http = ferrocast_twitch::http_client();
        let who = twitch_auth::validate(&http, &ep, &tokens.access_token)
            .await
            .map_err(|e| EngineError::Unavailable(e.to_string()))?;
        self.save_twitch_tokens(&tokens)?;
        let auth = self.twitch_auth(tokens)?;
        let me = ferrocast_twitch::helix::Helix::new(auth.clone()).me().await.ok();
        let account = repo::settings::Account {
            platform: "twitch".into(),
            user_id: who.user_id.clone(),
            login: who.login.clone(),
            display_name: me.as_ref().map(|u| u.display_name.clone()).unwrap_or(who.login.clone()),
            avatar_url: me.and_then(|u| (!u.profile_image_url.is_empty()).then_some(u.profile_image_url)),
            scopes: who.scopes,
            connected_at: now().0,
        };
        let saved = account.clone();
        self.db(move |s| repo::settings::save_account(s, &saved)).await?;
        self.start_twitch(auth, account);
        self.notice(NoticeLevel::Info, "Twitch is connected.");
        Ok(())
    }

    fn save_twitch_tokens(&self, t: &Tokens) -> Result<(), EngineError> {
        let json = serde_json::to_string(t).map_err(|e| EngineError::Internal(e.to_string()))?;
        self.inner.secrets.set(TWITCH_TOKENS, &json).map_err(|e| EngineError::Unavailable(e.to_string()))
    }

    fn twitch_auth(&self, tokens: Tokens) -> Result<Arc<Auth>, EngineError> {
        let client_id =
            self.apps().twitch_client_id.clone().ok_or_else(|| EngineError::Unavailable("no Twitch app".into()))?;
        let secrets = self.inner.secrets.clone();
        let persist = Arc::new(move |t: &Tokens| {
            let json = serde_json::to_string(t).map_err(|e| e.to_string())?;
            secrets.set(TWITCH_TOKENS, &json).map_err(|e| e.to_string())
        });
        Ok(Arc::new(Auth::new(
            ferrocast_twitch::http_client(),
            self.apps().twitch_endpoints.clone(),
            client_id,
            tokens,
            persist,
        )))
    }

    /// Registers the connector, marks the account in the status strip, and runs EventSub.
    fn start_twitch(&self, auth: Arc<Auth>, account: repo::settings::Account) {
        let helix = ferrocast_twitch::helix::Helix::new(auth);
        self.register_connector(Arc::new(TwitchConnector { helix: helix.clone(), user_id: account.user_id.clone() }));
        {
            let mut st = self.state_mut();
            let v = st.connectors.entry("twitch".into()).or_insert_with(crate::state::ConnectorView::off);
            v.account = Some(crate::state::AccountView {
                name: account.display_name.clone(),
                avatar_url: account.avatar_url.clone(),
            });
            v.state = "reconnecting".into();
        }
        let url = self.apps().twitch_endpoints.eventsub.clone();
        let handle = tokio::spawn(ferrocast_twitch::eventsub::run(helix, account.user_id, url, self.sink()));
        if let Some(old) =
            self.inner.adapters.lock().unwrap_or_else(|p| p.into_inner()).insert(Platform::Twitch, handle)
        {
            old.abort();
        }
    }

    /// Starts adapters for accounts saved from earlier runs.
    pub(crate) async fn resume_accounts(&self) {
        let Ok(accounts) = self.db(repo::settings::accounts).await else {
            return;
        };
        for a in accounts {
            if a.platform == "twitch" {
                let saved = self.inner.secrets.get(TWITCH_TOKENS).ok().flatten();
                match saved.and_then(|j| serde_json::from_str::<Tokens>(&j).ok()) {
                    Some(t) => match self.twitch_auth(t) {
                        Ok(auth) => self.start_twitch(auth, a),
                        Err(e) => tracing::warn!(%e, "Twitch is saved but can't start"),
                    },
                    None => {
                        let _ = self
                            .submit(Event::live(
                                Platform::Twitch,
                                None,
                                Millis(0),
                                EventKind::Connector(ConnectorStatus::SignInNeeded),
                            ))
                            .await;
                    }
                }
            }
        }
    }

    /// Signs a platform out: stops its adapter, forgets its tokens and account.
    pub async fn disconnect_account(&self, platform: Platform) -> Result<(), EngineError> {
        if let Some(h) = self.inner.adapters.lock().unwrap_or_else(|p| p.into_inner()).remove(&platform) {
            h.abort();
        }
        self.unregister_connector(platform);
        if platform == Platform::Twitch {
            self.inner.secrets.delete(TWITCH_TOKENS).map_err(|e| EngineError::Unavailable(e.to_string()))?;
        }
        let key = crate::platform_key(platform).to_string();
        self.db(move |s| repo::settings::remove_account(s, &key)).await?;
        Ok(())
    }

    pub(crate) fn notice(&self, level: NoticeLevel, message: &str) {
        self.send_live(LiveFrame::new("notice", &Notice { level, message: message.to_string(), action: None }));
    }
}
