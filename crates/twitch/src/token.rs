//! Holds the signed-in tokens, refreshes them once at a time, and saves every new pair
//! before it's used (refresh tokens are single-use).

use std::fmt;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::auth::{self, Tokens};
use crate::{Endpoints, TwitchError, now_ms};

type OnTokens = Arc<dyn Fn(&Tokens) -> Result<(), String> + Send + Sync>;

pub struct Auth {
    pub(crate) http: reqwest::Client,
    pub(crate) ep: Endpoints,
    pub(crate) client_id: String,
    tokens: Mutex<Tokens>,
    on_tokens: OnTokens,
}

impl fmt::Debug for Auth {
    // Never print tokens.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth").field("client_id", &self.client_id).finish_non_exhaustive()
    }
}

impl Auth {
    /// `on_tokens` must persist new tokens (the secret store) and return an error if it couldn't;
    /// a refresh whose result can't be saved would lose the account at the next restart.
    pub fn new(http: reqwest::Client, ep: Endpoints, client_id: String, tokens: Tokens, on_tokens: OnTokens) -> Self {
        Auth { http, ep, client_id, tokens: Mutex::new(tokens), on_tokens }
    }

    /// A valid access token, refreshing first when it's about to expire.
    pub async fn access(&self) -> Result<String, TwitchError> {
        let mut t = self.tokens.lock().await;
        if t.needs_refresh(now_ms()) {
            *t = self.refresh_locked(&t).await?;
        }
        Ok(t.access_token.clone())
    }

    /// Refreshes after a 401, unless another task already did since `seen` was handed out.
    pub async fn refresh_after_401(&self, seen: &str) -> Result<String, TwitchError> {
        let mut t = self.tokens.lock().await;
        if t.access_token != seen {
            return Ok(t.access_token.clone());
        }
        *t = self.refresh_locked(&t).await?;
        Ok(t.access_token.clone())
    }

    pub async fn scopes(&self) -> Vec<String> {
        self.tokens.lock().await.scopes.clone()
    }

    async fn refresh_locked(&self, current: &Tokens) -> Result<Tokens, TwitchError> {
        let fresh = auth::refresh(&self.http, &self.ep, &self.client_id, &current.refresh_token).await?;
        (self.on_tokens)(&fresh)
            .map_err(|e| TwitchError::Refused(format!("couldn't save the new Twitch sign-in: {e}")))?;
        tracing::info!("Twitch sign-in refreshed");
        Ok(fresh)
    }
}
