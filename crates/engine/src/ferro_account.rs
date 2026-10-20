//! The streamer's Ferrocast account (ACCOUNTS.md §3, §5): optional. The app works the same
//! without it; signing in only ties Pro to the account so it follows them to every PC.
//!
//! Sign-in is the loopback redirect with PKCE (RFC 8252): the engine listens on
//! `127.0.0.1:<random port>`, asks the relay to start a flow, the dashboard opens the URL in
//! the browser, the relay redirects back with a one-time code, and the engine trades it for
//! an app session token. The token lives in the secret store (R-45), never in settings.

use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::frames::{LiveFrame, NoticeLevel};
use crate::{Engine, EngineError, now};

/// The app session token (`fca_…`).
pub(crate) const SESSION: &str = "ferrocast-account-session";
/// The last account answer, for when the relay can't be reached (no secrets in it).
const CACHE_KEY: &str = "ferrocast_account";
/// The relay keeps an app sign-in open for 10 minutes.
const SIGN_IN_WAIT: Duration = Duration::from_secs(600);
/// A relay answer this recent is reused by `GET /pro/account` (without `refresh`).
const FRESH_MS: i64 = 15 * 60 * 1000;
/// The account is read again this often (keeps the session alive and Pro current).
const REFRESH_EVERY: Duration = Duration::from_secs(4 * 3600);

/// A sign-in in progress, or the last one's problem.
#[derive(Debug, Default)]
pub(crate) struct Flow {
    task: Option<tokio::task::JoinHandle<()>>,
    /// Which sign-in this is: a finished task only clears its own.
    generation: u64,
    started_at: i64,
    url: Option<String>,
    expires_at: Value,
    error: Option<(String, String)>,
}

fn random_b64(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    // No OS randomness means nothing secure is possible; refuse loudly.
    #[allow(clippy::expect_used)]
    getrandom::fill(&mut b).expect("the OS random number generator is unavailable");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// The PKCE pair: a 64-character verifier and its S256 challenge.
pub(crate) fn pkce() -> (String, String) {
    let verifier = random_b64(48);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn refused(status: u16, code: &'static str, message: &str, hint: &str) -> EngineError {
    EngineError::Refused { status, code, message: message.into(), hint: hint.into(), retry_after: None }
}

/// The OS keychain (the secret store) refused: a clear 503, never the store's own words alone.
fn keychain(e: impl std::fmt::Display) -> EngineError {
    refused(
        503,
        "keychain_unavailable",
        &format!("Ferrocast couldn't use this computer's keychain: {e}"),
        "Unlock the keychain (or allow Ferrocast to use it), then try again.",
    )
}

/// Account calls answer 503 when the relay can't be reached or this build has none.
fn offline(e: EngineError) -> EngineError {
    match e {
        EngineError::Refused { code: "relay_unreachable", message, hint, .. } => {
            EngineError::Refused { status: 503, code: "relay_unreachable", message, hint, retry_after: None }
        }
        EngineError::Unavailable(m) if m.contains("no relay") => refused(
            503,
            "no_relay",
            "This build of Ferrocast has no relay set up, so it can't sign in to an account.",
            "Use the official build from ferrocast.clearcraftlabs.com/download.",
        ),
        other => other,
    }
}

/// Compares two strings in time that doesn't depend on where they differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Background upkeep: at start and every few hours, read the account (keeps the session
/// alive, Pro current) and keep the relay link open while signed in, so the web
/// dashboard's `remote_wanted` reaches this PC.
pub(crate) async fn run(engine: Engine) {
    if engine.relay_url().is_err() {
        return;
    }
    loop {
        if engine.account_signed_in() {
            engine.ensure_relay_link();
            let v = engine.account_status(true).await;
            engine.send_live(LiveFrame::new("account", &v));
        }
        tokio::time::sleep(REFRESH_EVERY).await;
    }
}

impl Engine {
    fn flow(&self) -> std::sync::MutexGuard<'_, Flow> {
        self.inner.account_flow.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn account_token(&self) -> Result<Option<String>, EngineError> {
        self.inner.secrets.get(SESSION).map_err(keychain)
    }

    /// Signed in to a Ferrocast account here (from the keychain, no network).
    pub(crate) fn account_signed_in(&self) -> bool {
        self.account_token().ok().flatten().is_some()
    }

    /// The signed-in account's email, as last read (for checkout without typing it).
    pub async fn account_email(&self) -> Option<String> {
        if !self.account_signed_in() {
            return None;
        }
        let cached: Option<Value> =
            self.db(|s| ferrocast_store::repo::settings::get::<Value>(s, CACHE_KEY)).await.ok().flatten();
        cached?.pointer("/account/email").and_then(Value::as_str).filter(|e| e.contains('@')).map(String::from)
    }

    /// Reads the account again in the background (after checkout, redeem, a Pro refresh)
    /// and tells the dashboard.
    pub(crate) fn account_refresh_soon(&self) {
        if !self.account_signed_in() {
            return;
        }
        let engine = self.clone();
        tokio::spawn(async move {
            let v = engine.account_status(true).await;
            engine.send_live(LiveFrame::new("account", &v));
        });
    }

    async fn account_forget(&self) {
        if let Err(e) = self.inner.secrets.delete(SESSION) {
            tracing::warn!(error = %e, "couldn't forget the account session");
        }
        let _ = self.db(|s| ferrocast_store::repo::settings::set(s, CACHE_KEY, &Value::Null)).await;
    }

    /// Calls the relay's `/v1/account…` with the app session token.
    async fn account_call(
        &self,
        token: &str,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value), EngineError> {
        let url = self.relay_url().map_err(offline)?;
        let mut req = self.relay_http().request(method, format!("{url}{path}")).bearer_auth(token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await.map_err(|e| {
            refused(
                503,
                "relay_unreachable",
                &format!("Couldn't reach Ferrocast's servers: {}", e.without_url()),
                "Check your internet connection and try again.",
            )
        })?;
        let status = res.status().as_u16();
        if status == 401 {
            // The session ended (signed out elsewhere, revoked or expired): forget it here too.
            self.account_forget().await;
            // Pro may have come from the account; the tunnel may no longer be allowed.
            tokio::spawn(after_signed_out(self.clone()));
            return Err(refused(
                401,
                "signed_out",
                "You're signed out of your Ferrocast account.",
                "Sign in again to bring your account's Pro back to this PC.",
            ));
        }
        if !(200..300).contains(&status) {
            let wait = res
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok());
            let v: Value = res.json().await.unwrap_or(Value::Null);
            return Err(crate::pro::relay_error_parts(status, wait, &v));
        }
        if status == 204 {
            return Ok((status, Value::Null));
        }
        let v = res.json().await.unwrap_or(Value::Null);
        Ok((status, v))
    }

    /// `GET /pro/account`: signed out, signing in, signed in (the relay's answer) or disabled.
    /// A relay answer from the last 15 minutes is reused unless `refresh`; offline it answers
    /// from memory, marked `stale`. Never an error.
    pub async fn account_status(&self, refresh: bool) -> Value {
        {
            let f = self.flow();
            if f.task.as_ref().is_some_and(|t| !t.is_finished()) {
                return json!({ "state": "pending", "started_at": f.started_at, "url": f.url,
                               "expires_at": f.expires_at });
            }
        }
        let last_error = self.flow().error.clone().map(|(m, h)| json!({ "message": m, "hint": h }));
        let token = match self.account_token() {
            Ok(Some(t)) => t,
            Ok(None) => return json!({ "state": "signed_out", "error": last_error }),
            Err(e) => {
                let (message, hint) = match e {
                    EngineError::Refused { message, hint, .. } => (message, hint),
                    other => (other.to_string(), String::new()),
                };
                return json!({ "state": "signed_out", "error": { "message": message, "hint": hint } });
            }
        };
        if !refresh {
            let cached: Option<Value> =
                self.db(|s| ferrocast_store::repo::settings::get::<Value>(s, CACHE_KEY)).await.ok().flatten();
            if let Some(c) = cached.filter(|c| c["state"] == "signed_in" && c["stale"] == false)
                && now().0.saturating_sub(c["checked_at"].as_i64().unwrap_or(0)) < FRESH_MS
            {
                return c;
            }
        }
        match self.account_call(&token, reqwest::Method::GET, "/v1/account", None).await {
            Ok((_, mut v)) => {
                if let Some(o) = v.as_object_mut() {
                    o.remove("csrf_token");
                    o.insert("state".into(), json!("signed_in"));
                    o.insert("stale".into(), json!(false));
                    o.insert("checked_at".into(), json!(now().0));
                }
                let saved = v.clone();
                let _ = self.db(move |s| ferrocast_store::repo::settings::set(s, CACHE_KEY, &saved)).await;
                v
            }
            Err(EngineError::Refused { code: "signed_out", message, hint, .. }) => {
                json!({ "state": "signed_out", "error": { "message": message, "hint": hint } })
            }
            Err(EngineError::Refused { code: "account_disabled", message, hint, .. }) => {
                json!({ "state": "disabled", "error": { "message": message, "hint": hint } })
            }
            Err(e) => {
                let cached: Option<Value> =
                    self.db(|s| ferrocast_store::repo::settings::get::<Value>(s, CACHE_KEY)).await.ok().flatten();
                match cached.filter(|c| c.is_object()) {
                    Some(mut c) => {
                        c["stale"] = json!(true);
                        c["error"] = json!({ "message": e.to_string(), "hint": "Showing what Ferrocast last saw." });
                        c
                    }
                    None => json!({ "state": "signed_in", "stale": true, "account": null,
                                    "error": { "message": e.to_string(), "hint": "Try again in a moment." } }),
                }
            }
        }
    }

    /// `POST /pro/account/sign-in` → `{ url }` to open in the browser. The engine waits for
    /// the browser to come back (10 minutes) by itself.
    pub async fn account_sign_in(&self, label: Option<String>) -> Result<Value, EngineError> {
        if self.account_token()?.is_some() {
            return Err(refused(
                409,
                "already_signed_in",
                "This PC is already signed in to a Ferrocast account.",
                "Sign out first to switch accounts.",
            ));
        }
        let relay = self.relay_url().map_err(offline)?;
        self.account_sign_in_cancel();
        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|e| {
            refused(
                503,
                "listener_unavailable",
                &format!("Ferrocast couldn't listen for the browser on this PC: {e}"),
                "Check that a firewall or security app allows Ferrocast on 127.0.0.1, then try again.",
            )
        })?;
        let port = listener
            .local_addr()
            .map_err(|e| refused(503, "listener_unavailable", &e.to_string(), "Try again."))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}/auth/callback");
        let (verifier, challenge) = pkce();
        let state = random_b64(24);
        let label: String = label.unwrap_or_default().trim().chars().take(40).collect();
        let mut body = json!({ "redirect_uri": redirect_uri, "code_challenge": challenge,
                               "code_challenge_method": "S256", "state": state });
        if !label.is_empty() {
            body["label"] = json!(label);
        }
        let (_, v) =
            self.pro_call_status(reqwest::Method::POST, "/v1/account/app/start", Some(body)).await.map_err(offline)?;
        // Only ever send the browser to the relay's own kind of address (https, or http for a
        // local test relay), never a scheme that starts another program.
        let https_only = relay.starts_with("https://");
        let url = v["url"]
            .as_str()
            .filter(|u| u.len() <= 2048)
            .filter(|u| u.starts_with("https://") || (!https_only && u.starts_with("http://")));
        let Some(url) = url.map(String::from) else {
            return Err(refused(502, "relay_error", "The relay sent no sign-in address.", "Try again in a moment."));
        };
        let mut f = self.flow();
        // Another sign-in started meanwhile: this one replaces it.
        if let Some(t) = f.task.take() {
            t.abort();
        }
        let generation = f.generation.wrapping_add(1);
        let engine = self.clone();
        let task = tokio::spawn(async move {
            let res = engine.account_wait(listener, &state, &verifier, SIGN_IN_WAIT).await;
            {
                let mut f = engine.flow();
                if f.generation != generation {
                    return;
                }
                f.task = None;
                f.error = res.err().map(|e| match e {
                    EngineError::Refused { message, hint, .. } => (message, hint),
                    other => (other.to_string(), "Press Sign in to try again.".into()),
                });
            }
            engine.send_live(LiveFrame::new("account", &engine.account_status(true).await));
        });
        *f = Flow {
            task: Some(task),
            generation,
            started_at: now().0,
            url: Some(url.clone()),
            expires_at: v["expires_at"].clone(),
            error: None,
        };
        drop(f);
        self.send_live(LiveFrame::new(
            "account",
            &json!({ "state": "pending", "started_at": now().0, "url": url, "expires_at": v["expires_at"] }),
        ));
        Ok(json!({ "url": url, "expires_at": v["expires_at"] }))
    }

    /// Stops waiting for the browser (the Cancel link). Dropping the task closes the listener.
    pub fn account_sign_in_cancel(&self) {
        let was_waiting = {
            let mut f = self.flow();
            f.generation = f.generation.wrapping_add(1);
            f.error = None;
            f.url = None;
            f.task.take().map(|t| t.abort()).is_some()
        };
        if was_waiting {
            self.send_live(LiveFrame::new("account", &json!({ "state": "signed_out", "error": null })));
        }
    }

    async fn account_wait(
        &self,
        listener: TcpListener,
        state: &str,
        verifier: &str,
        wait: Duration,
    ) -> Result<(), EngineError> {
        let (code, mut socket) = wait_for_code(listener, state, wait).await?;
        let res = self.account_trade(&code, verifier).await;
        let page = match &res {
            Ok(()) => (200, "You're signed in. Go back to Ferrocast; you can close this tab.".to_string()),
            Err(EngineError::Refused { message, hint, .. }) => (400, format!("{message} {hint}")),
            Err(e) => (400, format!("{e} Go back to Ferrocast and press Sign in again.")),
        };
        let _ = respond(&mut socket, page.0, &page.1).await;
        res
    }

    /// Trades the one-time code for the app session and keeps it in the keychain.
    async fn account_trade(&self, code: &str, verifier: &str) -> Result<(), EngineError> {
        let (_, v) = self
            .pro_call_status(
                reqwest::Method::POST,
                "/v1/account/app/token",
                Some(json!({ "code": code, "code_verifier": verifier })),
            )
            .await
            .map_err(|e| match e {
                // Every miss (wrong, used or expired code, wrong verifier) reads the same.
                // (The relay's 410 arrives as 502: match the code, not the status.)
                EngineError::Refused { code: code @ ("flow_expired" | "invalid_challenge"), .. } => refused(
                    if code == "flow_expired" { 410 } else { 422 },
                    code,
                    "The sign-in code didn't work.",
                    "Press Sign in to try again.",
                ),
                other => offline(other),
            })?;
        let token =
            v["session_token"].as_str().filter(|t| t.starts_with("fca_") && t.len() <= 512).ok_or_else(|| {
                refused(502, "relay_error", "The relay didn't send a session.", "Press Sign in to try again.")
            })?;
        self.inner.secrets.set(SESSION, token).map_err(keychain)?;
        let email = v.pointer("/account/email").and_then(Value::as_str).unwrap_or("your account").to_string();
        // Pro may now come from the account (the relay decides); the relay link carries the
        // web dashboard's `remote_wanted`, and the tunnel may open without Pro now.
        let _ = self.pro_status(true).await;
        self.ensure_relay_link();
        self.set_remote_wanted(true).await;
        self.notice("ferrocast-account", NoticeLevel::Info, &format!("Signed in as {email}."));
        Ok(())
    }

    /// `POST /pro/account/sign-out {everywhere}`. Forgets the session here even when the
    /// relay can't be reached (it expires there on its own).
    pub async fn account_sign_out(&self, everywhere: bool) -> Result<(), EngineError> {
        self.account_sign_in_cancel();
        let Some(token) = self.account_token()? else { return Ok(()) };
        let res = self
            .account_call(
                &token,
                reqwest::Method::POST,
                "/v1/account/logout",
                Some(json!({ "everywhere": everywhere })),
            )
            .await;
        self.account_forget().await;
        let _ = self.pro_status(true).await;
        self.remote_wake();
        self.send_live(LiveFrame::new("account", &json!({ "state": "signed_out", "error": null })));
        match res {
            Ok(_) | Err(EngineError::Refused { code: "signed_out", .. }) => Ok(()),
            // Signed out here; only "everywhere" needs the relay to have heard it.
            Err(e) if everywhere => Err(offline(e)),
            Err(_) => Ok(()),
        }
    }

    /// `GET /pro/account/sessions`: the account's signed-in apps and browsers.
    pub async fn account_sessions(&self) -> Result<Value, EngineError> {
        let token = self.account_token()?.ok_or_else(not_signed_in)?;
        let mut v = self.account_call(&token, reqwest::Method::GET, "/v1/account/sessions", None).await?.1;
        if !v["sessions"].is_array() {
            v = json!({ "sessions": [] });
        }
        Ok(v)
    }

    /// `DELETE /pro/account/sessions/{id}`.
    pub async fn account_revoke(&self, id: &str) -> Result<(), EngineError> {
        if id.is_empty() || id.len() > 80 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(EngineError::Invalid("That session id doesn't look right.".into()));
        }
        let token = self.account_token()?.ok_or_else(not_signed_in)?;
        self.account_call(&token, reqwest::Method::DELETE, &format!("/v1/account/sessions/{id}"), None).await?;
        // Revoking this PC's own session signs it out (the read answers 401 and forgets it).
        if token.strip_prefix("fca_").and_then(|t| t.split('.').next()) == Some(id) {
            let _ = self.account_status(true).await;
        }
        Ok(())
    }
}

/// After the relay said the session ended: Pro again, the tunnel, the dashboard. Boxed:
/// it reads the account, which may end up here again.
fn after_signed_out(engine: Engine) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move {
        let _ = engine.pro_status(true).await;
        engine.remote_wake();
        let v = json!({ "state": "signed_out", "error": {
            "message": "You're signed out of your Ferrocast account.",
            "hint": "Sign in again to bring your account's Pro back to this PC." } });
        engine.send_live(LiveFrame::new("account", &v));
    })
}

fn not_signed_in() -> EngineError {
    refused(401, "signed_out", "This PC isn't signed in to a Ferrocast account.", "Press Sign in first.")
}

/// Waits for the browser's redirect to `/auth/callback`, checks `state`, returns the code
/// and the browser's connection, answered once the code is traded.
async fn wait_for_code(
    listener: TcpListener,
    state: &str,
    wait: Duration,
) -> Result<(String, tokio::net::TcpStream), EngineError> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let (mut socket, _) = tokio::time::timeout_at(deadline, listener.accept())
            .await
            .map_err(|_| refused(410, "flow_expired", "Signing in took too long.", "Press Sign in to try again."))?
            .map_err(|e| refused(503, "listener_unavailable", &e.to_string(), "Press Sign in to try again."))?;
        let mut buf = vec![0u8; 8192];
        let n = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buf))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        let target = request.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("");
        let parsed = reqwest::Url::parse(&format!("http://127.0.0.1{target}")).ok();
        let q = |k: &str| {
            parsed.as_ref().and_then(|u| u.query_pairs().find(|(key, _)| key == k).map(|(_, v)| v.into_owned()))
        };
        if parsed.as_ref().map(|u| u.path()) != Some("/auth/callback") {
            let _ = respond(&mut socket, 404, "Not found").await;
            continue; // the browser asking for /favicon.ico
        }
        // A stray request with the wrong state never ends the real sign-in.
        if !q("state").is_some_and(|s| same(&s, state)) {
            let _ = respond(
                &mut socket,
                400,
                "This sign-in link doesn't match. Go back to Ferrocast and press Sign in again.",
            )
            .await;
            continue;
        }
        if q("error").is_some() {
            let _ = respond(&mut socket, 200, "Sign-in was cancelled. You can close this tab.").await;
            return Err(refused(
                400,
                "cancelled",
                "Sign-in was cancelled in the browser.",
                "Press Sign in to try again.",
            ));
        }
        match q("code") {
            Some(code) if !code.is_empty() && code.len() <= 512 => return Ok((code, socket)),
            _ => {
                let _ = respond(&mut socket, 400, "No sign-in code arrived. Press Sign in again.").await;
                return Err(refused(502, "relay_error", "No sign-in code arrived.", "Press Sign in to try again."));
            }
        }
    }
}

async fn respond(socket: &mut tokio::net::TcpStream, status: u16, message: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Ferrocast</title><body style=\"font:16px system-ui;background:#0a0b0d;color:#eef0f4;display:grid;place-items:center;height:100vh;margin:0\"><p>{}</p>",
        message.replace('&', "&amp;").replace('<', "&lt;")
    );
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'\r\n\r\n",
        body.len()
    );
    socket.write_all(head.as_bytes()).await?;
    socket.write_all(body.as_bytes()).await?;
    socket.shutdown().await
}

#[cfg(test)]
#[path = "ferro_account_tests.rs"]
mod tests;
