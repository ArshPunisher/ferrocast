//! Signing platforms in and out, and running their adapters.
//!
//! Tokens live in the secret store (R-45); the `accounts` table only remembers who is
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
const YOUTUBE_TOKENS: &str = "youtube-tokens";
/// How long the browser sign-in for YouTube may take.
const YOUTUBE_SIGN_IN: Duration = Duration::from_secs(300);

/// The public client ids of Ferrocast's platform apps. They're public by design (device
/// code / PKCE flows); release builds bake them in, and `FERROCAST_TWITCH_CLIENT_ID` etc.
/// override them for development (R-46: never a secret in the app).
#[derive(Debug, Clone, Default)]
pub struct PlatformApps {
    pub twitch_client_id: Option<String>,
    pub twitch_endpoints: Endpoints,
    pub youtube: Option<ferrocast_youtube::AppCredentials>,
    pub youtube_endpoints: ferrocast_youtube::Endpoints,
    /// The Ferrocast relay (Kick webhooks and sign-in, later tips).
    pub relay_url: Option<String>,
    /// Kick's public API (tests point it at a fake).
    pub kick_api: String,
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
            youtube: match (
                pick("FERROCAST_YOUTUBE_CLIENT_ID", option_env!("FERROCAST_YOUTUBE_CLIENT_ID")),
                pick("FERROCAST_YOUTUBE_CLIENT_SECRET", option_env!("FERROCAST_YOUTUBE_CLIENT_SECRET")),
            ) {
                (Some(client_id), Some(client_secret)) => {
                    Some(ferrocast_youtube::AppCredentials { client_id, client_secret })
                }
                _ => None,
            },
            youtube_endpoints: ferrocast_youtube::Endpoints::default(),
            relay_url: pick("FERROCAST_RELAY_URL", option_env!("FERROCAST_RELAY_URL"))
                .map(|u| u.trim_end_matches('/').to_string()),
            kick_api: "https://api.kick.com".into(),
        }
    }
}

/// What the dashboard shows while the streamer signs in.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "flow", rename_all = "snake_case")]
pub enum ConnectFlow {
    DeviceCode {
        user_code: String,
        verification_url: String,
        expires_at: i64,
    },
    /// Open this URL in the system browser; the engine is waiting for Google to send the
    /// streamer back.
    Browser {
        url: String,
    },
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
            Platform::Youtube => self.connect_youtube().await,
            Platform::Kick => self.connect_kick().await,
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
                    engine.notice(
                        "account:twitch",
                        NoticeLevel::Warn,
                        "The Twitch sign-in code expired. Press Connect again.",
                    );
                    return;
                }
                match twitch_auth::poll(&http, &ep, &client_id, &code.device_code).await {
                    Ok(Poll::Pending) => {}
                    Ok(Poll::SlowDown) => interval += Duration::from_secs(5),
                    Ok(Poll::Done(tokens)) => {
                        if let Err(e) = engine.finish_twitch(tokens).await {
                            engine.notice(
                                "account:twitch",
                                NoticeLevel::Error,
                                &format!("Twitch sign-in didn't finish: {e}"),
                            );
                        }
                        return;
                    }
                    Err(e) => {
                        engine.notice("account:twitch", NoticeLevel::Warn, &e.to_string());
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
        self.notice("account:twitch", NoticeLevel::Info, "Twitch is connected.");
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
        // Its token proves the channel when connecting the cloud bot (cloud_bot.rs).
        self.cloud_bot_set_twitch(Some(auth.clone()));
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
            if a.platform == "kick" {
                self.resume_kick(a.clone()).await;
                continue;
            }
            if a.platform == "youtube" {
                let saved = self.inner.secrets.get(YOUTUBE_TOKENS).ok().flatten();
                match saved.and_then(|j| serde_json::from_str::<ferrocast_youtube::Tokens>(&j).ok()) {
                    Some(t) => match self.youtube_api(t) {
                        Ok(api) => self.start_youtube(api, a.clone()),
                        Err(e) => tracing::warn!(%e, "YouTube is saved but can't start"),
                    },
                    None => {
                        let _ = self
                            .submit(Event::live(
                                Platform::Youtube,
                                None,
                                Millis(0),
                                EventKind::Connector(ConnectorStatus::SignInNeeded),
                            ))
                            .await;
                    }
                }
                continue;
            }
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
        let key = match platform {
            Platform::Twitch => {
                self.cloud_bot_set_twitch(None);
                Some(TWITCH_TOKENS)
            }
            Platform::Youtube => {
                *self.inner.youtube_wake.lock().unwrap_or_else(|p| p.into_inner()) = None;
                self.cloud_bot_set_youtube(None);
                Some(YOUTUBE_TOKENS)
            }
            Platform::Kick => {
                self.forget_kick()?;
                None
            }
            _ => None,
        };
        if let Some(k) = key {
            self.inner.secrets.delete(k).map_err(|e| EngineError::Unavailable(e.to_string()))?;
        }
        let key = crate::platform_key(platform).to_string();
        self.db(move |s| repo::settings::remove_account(s, &key)).await?;
        Ok(())
    }

    async fn connect_youtube(&self) -> Result<ConnectFlow, EngineError> {
        let app = self.apps().youtube.clone().ok_or_else(|| {
            EngineError::Unavailable(
                "This build of Ferrocast has no YouTube app set up, so it can't sign in to YouTube.".into(),
            )
        })?;
        let ep = self.apps().youtube_endpoints.clone();
        let pending =
            ferrocast_youtube::oauth::begin(&ep, &app).await.map_err(|e| EngineError::Unavailable(e.to_string()))?;
        let url = pending.url.clone();
        let engine = self.clone();
        tokio::spawn(async move {
            let http = ferrocast_youtube::http_client();
            match ferrocast_youtube::oauth::finish(&http, &ep, &app, pending, YOUTUBE_SIGN_IN).await {
                Ok(tokens) => {
                    if let Err(e) = engine.finish_youtube(tokens).await {
                        engine.notice(
                            "account:youtube",
                            NoticeLevel::Error,
                            &format!("YouTube sign-in didn't finish: {e}"),
                        );
                    }
                }
                Err(e) => engine.notice("account:youtube", NoticeLevel::Warn, &e.to_string()),
            }
        });
        Ok(ConnectFlow::Browser { url })
    }

    async fn finish_youtube(&self, tokens: ferrocast_youtube::Tokens) -> Result<(), EngineError> {
        self.save_secret_json(YOUTUBE_TOKENS, &tokens)?;
        let api = self.youtube_api(tokens)?;
        let me = api.me().await.map_err(|e| EngineError::Unavailable(e.to_string()))?;
        let account = repo::settings::Account {
            platform: "youtube".into(),
            user_id: me.id.clone(),
            login: me.title.clone(),
            display_name: me.title.clone(),
            avatar_url: me.avatar_url.clone(),
            scopes: ferrocast_youtube::oauth::SCOPES.iter().map(|s| s.to_string()).collect(),
            connected_at: now().0,
        };
        let saved = account.clone();
        self.db(move |s| repo::settings::save_account(s, &saved)).await?;
        self.start_youtube(api, account);
        self.notice(
            "account:youtube",
            NoticeLevel::Info,
            "YouTube is connected. Chat shows up whenever you're live there.",
        );
        Ok(())
    }

    fn save_secret_json<T: Serialize>(&self, key: &str, value: &T) -> Result<(), EngineError> {
        let json = serde_json::to_string(value).map_err(|e| EngineError::Internal(e.to_string()))?;
        self.inner.secrets.set(key, &json).map_err(|e| EngineError::Unavailable(e.to_string()))
    }

    fn youtube_api(&self, tokens: ferrocast_youtube::Tokens) -> Result<Arc<ferrocast_youtube::Api>, EngineError> {
        let app = self.apps().youtube.clone().ok_or_else(|| EngineError::Unavailable("no YouTube app".into()))?;
        let secrets = self.inner.secrets.clone();
        let persist = Arc::new(move |t: &ferrocast_youtube::Tokens| {
            let json = serde_json::to_string(t).map_err(|e| e.to_string())?;
            secrets.set(YOUTUBE_TOKENS, &json).map_err(|e| e.to_string())
        });
        Ok(Arc::new(ferrocast_youtube::Api::new(
            ferrocast_youtube::http_client(),
            self.apps().youtube_endpoints.clone(),
            app,
            tokens,
            persist,
        )))
    }

    fn start_youtube(&self, api: Arc<ferrocast_youtube::Api>, account: repo::settings::Account) {
        let live: ferrocast_youtube::stream::LiveChat = Arc::default();
        // Its token proves the channel when connecting YouTube's cloud bot (cloud_bot.rs).
        self.cloud_bot_set_youtube(Some(crate::cloud_bot::DesktopYoutube { api: api.clone(), live: live.clone() }));
        self.register_connector(Arc::new(ferrocast_youtube::YoutubeConnector { api: api.clone(), live: live.clone() }));
        {
            let mut st = self.state_mut();
            let v = st.connectors.entry("youtube".into()).or_insert_with(crate::state::ConnectorView::off);
            v.account = Some(crate::state::AccountView {
                name: account.display_name.clone(),
                avatar_url: account.avatar_url.clone(),
            });
        }
        let wake = Arc::new(tokio::sync::Notify::new());
        *self.inner.youtube_wake.lock().unwrap_or_else(|p| p.into_inner()) = Some(wake.clone());
        let handle = tokio::spawn(ferrocast_youtube::stream::run(api, self.sink(), wake, live));
        if let Some(old) =
            self.inner.adapters.lock().unwrap_or_else(|p| p.into_inner()).insert(Platform::Youtube, handle)
        {
            old.abort();
        }
    }

    /// Another platform went live: ask YouTube to look for a live chat now, instead of
    /// waiting for its ten-minute check (saves quota and catches multistreams at once).
    pub(crate) fn wake_youtube(&self) {
        if let Some(w) = self.inner.youtube_wake.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            w.notify_one();
        }
    }

    /// One line on the dashboard. Notices with the same `key` replace each other there.
    pub(crate) fn notice(&self, key: &str, level: NoticeLevel, message: &str) {
        self.send_live(LiveFrame::new(
            "notice",
            &Notice { key: Some(key.to_string()), level, message: message.to_string(), action: None },
        ));
    }
}
