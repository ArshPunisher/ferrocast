//! Holds the signed-in tokens, refreshes them once at a time, and saves every new pair
//! before it's used (refresh tokens are single-use).

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
    /// The newest tokens couldn't be saved: saving is tried again at every use, so a keychain
    /// that was only briefly locked has them before the app quits (the saved refresh token is
    /// one Twitch already retired).
    unsaved: AtomicBool,
    /// When saving was last retried while `unsaved`: not again for a while, so a locked Keychain isn't
    /// asked, under the token lock, at every call.
    last_save_try: std::sync::Mutex<Option<std::time::Instant>>,
    /// Times in a row Helix refused a token just issued (reset when a call works).
    fresh_refused: std::sync::atomic::AtomicU32,
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
        Auth {
            http,
            ep,
            client_id,
            tokens: Mutex::new(tokens),
            on_tokens,
            unsaved: AtomicBool::new(false),
            last_save_try: std::sync::Mutex::new(None),
            fresh_refused: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// A valid access token, refreshing first when it's about to expire.
    pub async fn access(&self) -> Result<String, TwitchError> {
        let mut t = self.tokens.lock().await;
        if t.needs_refresh(now_ms()) {
            *t = self.refresh_locked(&t).await?;
        } else {
            self.save_again(&t);
        }
        Ok(t.access_token.clone())
    }

    /// Refreshes after a 401, unless another task already did since `seen` was handed out.
    pub async fn refresh_after_401(&self, seen: &str) -> Result<String, TwitchError> {
        let mut t = self.tokens.lock().await;
        if t.access_token != seen {
            self.save_again(&t);
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
        // Twitch has already retired the old refresh token: the new tokens are kept even when
        // saving them failed (a keychain hiccup), else the very next refresh would sign out for
        // good, and saved again at the next use (`save_again`).
        self.save(&fresh);
        tracing::info!("Twitch sign-in refreshed");
        Ok(fresh)
    }

    fn save(&self, t: &Tokens) {
        match (self.on_tokens)(t) {
            Ok(()) => self.unsaved.store(false, Ordering::Release),
            Err(e) => {
                // Logged once, not at every retry.
                if !self.unsaved.swap(true, Ordering::AcqRel) {
                    tracing::error!(error = %e, "couldn't save the new Twitch sign-in; it's kept for this session, saving is retried");
                }
            }
        }
    }

    /// Helix refused a token just issued: how many times in a row (this one included).
    pub(crate) fn note_fresh_refused(&self) -> u32 {
        self.fresh_refused.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// A call worked: the tokens are accepted.
    pub(crate) fn note_accepted(&self) {
        self.fresh_refused.store(0, Ordering::Release);
    }

    /// Lets the next use try saving again at once (tests; the wait is 30 s).
    #[cfg(test)]
    pub(crate) fn forget_save_cooldown(&self) {
        *self.last_save_try.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    fn save_again(&self, t: &Tokens) {
        if self.unsaved.load(Ordering::Acquire) {
            let mut last = self.last_save_try.lock().unwrap_or_else(|p| p.into_inner());
            if last.is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(30)) {
                *last = Some(std::time::Instant::now());
                self.save(t);
            }
        }
    }
}
