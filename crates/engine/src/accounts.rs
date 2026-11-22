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

/// The key of the notice [`Engine::browser_didnt_open`] sends.
pub const BROWSER_NOTICE: &str = "browser";

/// What the keychain holds for a saved sign-in: nothing, something unreadable, or the tokens.
/// A keychain that can't be read (locked, a hiccup at boot) is not the streamer being signed out:
/// the sign-in is kept and the connector isn't told "sign in again".
pub(crate) enum Saved<T> {
    Missing,
    Unreadable,
    Found(T),
}

pub(crate) fn saved_tokens<T: serde::de::DeserializeOwned>(
    secrets: &dyn ferrocast_secrets::Secrets,
    key: &str,
) -> Saved<T> {
    match secrets.get(key) {
        Err(e) => {
            tracing::warn!(error = %e, key, "the saved sign-in couldn't be read from the keychain; it is kept");
            Saved::Unreadable
        }
        Ok(None) => Saved::Missing,
        Ok(Some(j)) => serde_json::from_str(&j).map_or(Saved::Missing, Saved::Found),
    }
}

pub(crate) const TWITCH_TOKENS: &str = "twitch-tokens";
pub(crate) const YOUTUBE_TOKENS: &str = "youtube-tokens";
/// The Page Facebook acts as (its id and access token), in the secret store.
pub(crate) const FACEBOOK_PAGE: &str = "facebook-page";
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
    /// Facebook's Graph API (tests point it at a fake). Signing in to Facebook goes through the
    /// relay, which holds the Meta app secret: there is none here (R-46).
    pub facebook_endpoints: ferrocast_facebook::Endpoints,
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
            facebook_endpoints: ferrocast_facebook::Endpoints::default(),
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
            Platform::Facebook => self.connect_facebook().await,
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
                            engine.notice_detail(
                                "account:twitch",
                                NoticeLevel::Error,
                                "Twitch sign-in didn't finish",
                                &e.to_string(),
                            );
                        }
                        return;
                    }
                    Err(e) => {
                        engine.notice_detail(
                            "account:twitch",
                            NoticeLevel::Warn,
                            "Twitch sign-in failed",
                            &e.to_string(),
                        );
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
        let mut unreadable = Vec::new();
        for a in accounts {
            if self.resume_account(&a).await {
                unreadable.push(a);
            }
        }
        if unreadable.is_empty() {
            return;
        }
        // A keychain that can't be read (locked at boot, a hiccup) leaves those accounts off: say so,
        // and try again until it opens, instead of staying off without a word until the next start.
        let names: Vec<&str> = unreadable.iter().map(|a| a.platform.as_str()).collect();
        self.notice_detail(
            "accounts:keychain",
            NoticeLevel::Warn,
            "Your saved sign-ins can't be read yet",
            &format!(
                "The keychain didn't open ({}). Ferrocast keeps them and tries again every 30 seconds.",
                names.join(", ")
            ),
        );
        let engine = self.clone();
        tokio::spawn(async move {
            let mut left = unreadable;
            while !left.is_empty() {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let mut still = Vec::new();
                for a in left {
                    if engine.resume_account(&a).await {
                        still.push(a);
                    }
                }
                left = still;
            }
            engine.notice("accounts:keychain", NoticeLevel::Info, "Your saved sign-ins are back.");
        });
    }

    /// Starts one saved account's adapter. `true` when its keychain entry couldn't be read (nothing
    /// was done: it is kept, to be tried again).
    async fn resume_account(&self, a: &repo::settings::Account) -> bool {
        if a.platform == "kick" {
            return self.resume_kick(a.clone()).await;
        }
        if a.platform == "facebook" {
            match saved_tokens::<ferrocast_facebook::Page>(&*self.inner.secrets, FACEBOOK_PAGE) {
                Saved::Unreadable => return true,
                Saved::Found(page) => self.start_facebook(page, a.clone()),
                Saved::Missing => {
                    let _ = self
                        .submit(Event::live(
                            Platform::Facebook,
                            None,
                            Millis(0),
                            EventKind::Connector(ConnectorStatus::SignInNeeded),
                        ))
                        .await;
                }
            }
            return false;
        }
        if a.platform == "youtube" {
            match saved_tokens::<ferrocast_youtube::Tokens>(&*self.inner.secrets, YOUTUBE_TOKENS) {
                Saved::Unreadable => return true,
                Saved::Found(t) => match self.youtube_api(t) {
                    Ok(api) => self.start_youtube(api, a.clone()),
                    Err(e) => tracing::warn!(%e, "YouTube is saved but can't start"),
                },
                Saved::Missing => {
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
            return false;
        }
        if a.platform == "twitch" {
            match saved_tokens::<Tokens>(&*self.inner.secrets, TWITCH_TOKENS) {
                Saved::Unreadable => return true,
                Saved::Found(t) => match self.twitch_auth(t) {
                    Ok(auth) => self.start_twitch(auth, a.clone()),
                    Err(e) => tracing::warn!(%e, "Twitch is saved but can't start"),
                },
                Saved::Missing => {
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
        false
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
            Platform::Facebook => Some(FACEBOOK_PAGE),
            _ => None,
        };
        if let Some(k) = key {
            self.inner.secrets.delete(k).map_err(|e| EngineError::Unavailable(e.to_string()))?;
        }
        let key = crate::platform_key(platform).to_string();
        self.db(move |s| repo::settings::remove_account(s, &key)).await?;
        Ok(())
    }

    /// Facebook sign-in (a Page) through the relay, like Kick's: the relay asks Meta with the app
    /// secret (which only it holds) and hands the Pages to this install over its link, where
    /// [`Engine::finish_facebook`] takes the first one. Here the browser is opened on Meta's page.
    async fn connect_facebook(&self) -> Result<ConnectFlow, EngineError> {
        let url = self.relay_url()?;
        let auth = self.relay_auth().await?;
        self.ensure_relay_link();
        let res = self
            .relay_http()
            .post(format!("{url}/v1/facebook/oauth"))
            .bearer_auth(&auth)
            .send()
            .await
            .map_err(|e| EngineError::Unavailable(format!("Couldn't reach the Ferrocast relay: {e}")))?;
        let v: serde_json::Value = res.json().await.map_err(|e| EngineError::Unavailable(e.to_string()))?;
        match v["url"].as_str() {
            Some(u) => Ok(ConnectFlow::Browser { url: u.to_string() }),
            None => Err(EngineError::Unavailable(
                v.pointer("/error/message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("The relay couldn't start Facebook sign-in.")
                    .to_string(),
            )),
        }
    }

    /// The Page that was signed in to (the first one the person manages) becomes the account.
    pub(crate) async fn finish_facebook(&self, grant: ferrocast_facebook::UserGrant) -> Result<(), EngineError> {
        let Some(page) = grant.pages.into_iter().next() else {
            return Err(EngineError::Unavailable(
                "Facebook shows no Page that you manage. Going live and chat need a Page: make one, then Connect again.".into(),
            ));
        };
        self.save_secret_json(FACEBOOK_PAGE, &page)?;
        let account = repo::settings::Account {
            platform: "facebook".into(),
            user_id: page.id.clone(),
            login: page.name.clone(),
            display_name: page.name.clone(),
            avatar_url: page.avatar_url.clone(),
            scopes: ferrocast_facebook::SCOPES.iter().map(|s| s.to_string()).collect(),
            connected_at: now().0,
        };
        let saved = account.clone();
        self.db(move |s| repo::settings::save_account(s, &saved)).await?;
        self.start_facebook(page, account);
        self.notice_detail(
            "account:facebook",
            NoticeLevel::Info,
            "Facebook is connected",
            "Comments show up whenever your Page is live.",
        );
        Ok(())
    }

    /// Registers the connector and reads the Page's live comments.
    fn start_facebook(&self, page: ferrocast_facebook::Page, account: repo::settings::Account) {
        let graph = Arc::new(ferrocast_facebook::Graph::new(
            &self.apps().facebook_endpoints.graph,
            &page.id,
            &page.access_token,
        ));
        self.register_connector(Arc::new(ferrocast_facebook::FacebookConnector::new(graph.clone())));
        {
            let mut st = self.state_mut();
            let v = st.connectors.entry("facebook".into()).or_insert_with(crate::state::ConnectorView::off);
            v.account = Some(crate::state::AccountView {
                name: account.display_name.clone(),
                avatar_url: account.avatar_url.clone(),
            });
            // Comments are asked for, not pushed: connected means the Page is signed in.
            v.state = "connected".into();
            v.detail = None;
        }
        // Open dashboards (and a waiting "Connect Facebook" dialog) learn it now, not on reload.
        let view = self.state().connectors.clone();
        self.send_live(LiveFrame::new("status", &serde_json::json!({ "connectors": view })));
        let handle = tokio::spawn(ferrocast_facebook::feed::run(graph, self.sink(), None));
        if let Some(old) =
            self.inner.adapters.lock().unwrap_or_else(|p| p.into_inner()).insert(Platform::Facebook, handle)
        {
            old.abort();
        }
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
                        engine.notice_detail(
                            "account:youtube",
                            NoticeLevel::Error,
                            "YouTube sign-in didn't finish",
                            &e.to_string(),
                        );
                    }
                }
                Err(e) => {
                    engine.notice_detail("account:youtube", NoticeLevel::Warn, "YouTube sign-in failed", &e.to_string())
                }
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
        self.notice_detail(
            "account:youtube",
            NoticeLevel::Info,
            "YouTube is connected",
            "Chat shows up whenever you're live there.",
        );
        Ok(())
    }

    fn save_secret_json<T: Serialize>(&self, key: &str, value: &T) -> Result<(), EngineError> {
        let json = serde_json::to_string(value).map_err(|e| EngineError::Internal(e.to_string()))?;
        self.inner.secrets.set(key, &json).map_err(|e| EngineError::Unavailable(e.to_string()))
    }

    pub(crate) fn youtube_api(
        &self,
        tokens: ferrocast_youtube::Tokens,
    ) -> Result<Arc<ferrocast_youtube::Api>, EngineError> {
        let app = self.apps().youtube.clone().ok_or_else(|| EngineError::Unavailable("no YouTube app".into()))?;
        let secrets = self.inner.secrets.clone();
        let persist = Arc::new(move |t: &ferrocast_youtube::Tokens| {
            let json = serde_json::to_string(t).map_err(|e| e.to_string())?;
            secrets.set(YOUTUBE_TOKENS, &json).map_err(|e| e.to_string())
        });
        // The privacy of the broadcasts it makes is read from the settings each time, so a change
        // applies to the next one. A weak link: the Api lives inside the engine's connectors.
        let engine = Arc::downgrade(&self.inner);
        let privacy: ferrocast_youtube::PrivacySource = Arc::new(move || {
            // A value that isn't one of the three (a hand-edited file) must not publish: unlisted.
            engine
                .upgrade()
                .map(|inner| Engine { inner }.settings().youtube_privacy)
                .map_or(ferrocast_youtube::Privacy::Unlisted, |v| {
                    ferrocast_youtube::Privacy::parse(&v).unwrap_or(ferrocast_youtube::Privacy::Unlisted)
                })
        });
        Ok(Arc::new(
            ferrocast_youtube::Api::new(
                ferrocast_youtube::http_client(),
                self.apps().youtube_endpoints.clone(),
                app,
                tokens,
                persist,
            )
            .with_privacy(privacy),
        ))
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

    /// The desktop app couldn't hand a page (a sign-in page, a checkout) to the default
    /// browser. Every screen shows it; the connect dialog also offers its link to copy, so
    /// the person isn't left waiting for a browser that never opens.
    pub fn browser_didnt_open(&self) {
        self.notice_detail(
            BROWSER_NOTICE,
            NoticeLevel::Warn,
            "Ferrocast couldn't open your browser",
            "Try again in a moment, or open your browser first.",
        );
    }

    /// Turns the Studio's background saves into dashboard notices: one calm line per file that
    /// can't be saved (the OS error behind the (i)), replaced by a one-line "saving again" when it
    /// works. Call once at start-up; a later call replaces the earlier watcher.
    pub fn watch_saves(&self) {
        let engine = self.clone();
        ferrocast_store::durable::set_save_observer(move |e| engine.save_notice(e));
    }

    fn save_notice(&self, e: &ferrocast_store::durable::SaveEvent) {
        let (key, what) = save_subject(&e.file);
        match &e.error {
            Some(f) => {
                let notice = Notice::new(NoticeLevel::Warn, save_failure_message(what, f.kind, f.raw_os_error))
                    .key(key)
                    .detail(f.describe());
                self.send_live(LiveFrame::new("notice", &notice));
            }
            None => self.notice(&key, NoticeLevel::Info, &format!("Saving {what} again.")),
        }
    }

    /// One line on the dashboard. Notices with the same `key` replace each other there.
    pub(crate) fn notice(&self, key: &str, level: NoticeLevel, message: &str) {
        self.send_live(LiveFrame::new("notice", &Notice::new(level, message).key(key)));
    }

    /// A notice with an action the dashboard makes as a request (a POST of `body` to `href`).
    pub(crate) fn notice_post(
        &self,
        key: &str,
        level: NoticeLevel,
        message: &str,
        detail: &str,
        action: (&str, &str, serde_json::Value),
    ) {
        let n = Notice::new(level, message).key(key).detail(detail).post_action(action.0, action.1, action.2);
        self.send_live(LiveFrame::new("notice", &n));
    }

    /// The same with what the line leaves out (the engine's own words), behind an (i) on the page.
    pub(crate) fn notice_detail(&self, key: &str, level: NoticeLevel, message: &str, detail: &str) {
        self.send_live(LiveFrame::new("notice", &Notice::new(level, message).key(key).detail(detail)));
    }
}

/// A stable, non-personal key for a saved file (its folder and name, never the full path, which
/// holds the user's home folder) and what to call it.
fn save_subject(file: &std::path::Path) -> (String, &'static str) {
    let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let folder = file.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()).unwrap_or("");
    let what = match (folder, name) {
        ("collections", _) => "your scenes",
        ("profiles", _) => "your Studio settings",
        (_, "library.json") => "your list of scenes and settings",
        (_, "session.json") => "the Studio's session note",
        _ => "a Studio file",
    };
    (format!("save:{folder}/{name}"), what)
}

/// What to tell the person for a failed save, by why it failed.
fn save_failure_message(what: &str, kind: std::io::ErrorKind, raw: Option<i32>) -> String {
    use std::io::ErrorKind as K;
    // Windows: a file another program holds open (sharing 32, lock 33) or a busy one.
    if matches!(raw, Some(32 | 33)) || matches!(kind, K::ResourceBusy | K::WouldBlock) {
        return format!("Couldn't save {what}: the file is in use by another program.");
    }
    match kind {
        K::StorageFull | K::QuotaExceeded => format!("Couldn't save {what}: the disk is full. Free some space."),
        K::PermissionDenied => format!("Couldn't save {what}: Ferrocast isn't allowed to write there."),
        K::NotFound => format!("Couldn't save {what}: its folder is missing."),
        _ => format!("Couldn't save {what}."),
    }
}

#[cfg(test)]
mod save_notice_tests {
    use super::*;
    use std::io::ErrorKind as K;

    #[test]
    fn each_failure_has_its_own_plain_reason() {
        let m = |k, r| save_failure_message("your scenes", k, r);
        assert_eq!(m(K::StorageFull, None), "Couldn't save your scenes: the disk is full. Free some space.");
        assert_eq!(m(K::QuotaExceeded, None), "Couldn't save your scenes: the disk is full. Free some space.");
        assert_eq!(
            m(K::PermissionDenied, Some(13)),
            "Couldn't save your scenes: Ferrocast isn't allowed to write there."
        );
        assert_eq!(
            m(K::PermissionDenied, Some(32)),
            "Couldn't save your scenes: the file is in use by another program."
        );
        assert_eq!(
            m(K::PermissionDenied, Some(33)),
            "Couldn't save your scenes: the file is in use by another program."
        );
        assert_eq!(m(K::ResourceBusy, None), "Couldn't save your scenes: the file is in use by another program.");
        assert_eq!(m(K::NotFound, None), "Couldn't save your scenes: its folder is missing.");
        assert_eq!(m(K::InvalidData, None), "Couldn't save your scenes.");
    }

    #[test]
    fn the_key_has_no_home_folder_in_it() {
        let (key, what) = save_subject(std::path::Path::new("/Users/someone/Library/studio/collections/main.json"));
        assert_eq!((key.as_str(), what), ("save:collections/main.json", "your scenes"));
        assert!(!key.contains("someone"));
    }
}

#[cfg(test)]
mod saved_tokens_tests {
    use super::*;

    #[derive(Debug)]
    struct Locked;
    impl ferrocast_secrets::Secrets for Locked {
        fn get(&self, _: &str) -> Result<Option<String>, ferrocast_secrets::SecretError> {
            Err(ferrocast_secrets::SecretError::Unavailable("the keychain is locked".into()))
        }
        fn set(&self, _: &str, _: &str) -> Result<(), ferrocast_secrets::SecretError> {
            Ok(())
        }
        fn delete(&self, _: &str) -> Result<(), ferrocast_secrets::SecretError> {
            Ok(())
        }
    }

    #[test]
    fn a_keychain_that_cant_be_read_is_not_a_missing_sign_in() {
        assert!(matches!(saved_tokens::<Tokens>(&Locked, "twitch-tokens"), Saved::Unreadable));
        let mem = ferrocast_secrets::MemorySecrets::default();
        assert!(matches!(saved_tokens::<Tokens>(&mem, "twitch-tokens"), Saved::Missing));
    }
}
