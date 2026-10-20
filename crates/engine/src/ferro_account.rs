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

use crate::frames::NoticeLevel;
use crate::{Engine, EngineError, now};

/// The app session token (`fca_…`).
pub(crate) const SESSION: &str = "ferrocast-account-session";
/// The last account answer, for when the relay can't be reached (no secrets in it).
const CACHE_KEY: &str = "ferrocast_account";
/// The relay keeps an app sign-in open for 10 minutes.
const SIGN_IN_WAIT: Duration = Duration::from_secs(600);

/// A sign-in in progress, or the last one's problem.
#[derive(Debug, Default)]
pub(crate) struct Flow {
    task: Option<tokio::task::JoinHandle<()>>,
    started_at: i64,
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

impl Engine {
    fn flow(&self) -> std::sync::MutexGuard<'_, Flow> {
        self.inner.account_flow.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn account_token(&self) -> Result<Option<String>, EngineError> {
        self.inner.secrets.get(SESSION).map_err(|e| EngineError::Unavailable(e.to_string()))
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
        let url = self.relay_url()?;
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
    /// Offline it answers from memory, marked `stale`.
    pub async fn account_status(&self) -> Value {
        {
            let f = self.flow();
            if f.task.as_ref().is_some_and(|t| !t.is_finished()) {
                return json!({ "state": "pending", "started_at": f.started_at });
            }
        }
        let last_error = self.flow().error.clone().map(|(m, h)| json!({ "message": m, "hint": h }));
        let token = match self.account_token() {
            Ok(Some(t)) => t,
            Ok(None) => return json!({ "state": "signed_out", "error": last_error }),
            Err(e) => return json!({ "state": "signed_out", "error": { "message": e.to_string(), "hint": "" } }),
        };
        match self.account_call(&token, reqwest::Method::GET, "/v1/account", None).await {
            Ok((_, mut v)) => {
                if let Some(o) = v.as_object_mut() {
                    o.remove("csrf_token");
                    o.insert("state".into(), json!("signed_in"));
                    o.insert("stale".into(), json!(false));
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
            return Err(EngineError::Conflict(
                "This PC is already signed in. Sign out first to switch accounts.".into(),
            ));
        }
        self.account_sign_in_cancel();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| EngineError::Unavailable(format!("Couldn't open the sign-in listener: {e}")))?;
        let port = listener.local_addr().map_err(|e| EngineError::Unavailable(e.to_string()))?.port();
        let redirect_uri = format!("http://127.0.0.1:{port}/auth/callback");
        let (verifier, challenge) = pkce();
        let state = random_b64(24);
        let label: String = label.unwrap_or_default().trim().chars().take(40).collect();
        let mut body = json!({ "redirect_uri": redirect_uri, "code_challenge": challenge,
                               "code_challenge_method": "S256", "state": state });
        if !label.is_empty() {
            body["label"] = json!(label);
        }
        let (_, v) = self.pro_call_status(reqwest::Method::POST, "/v1/account/app/start", Some(body)).await?;
        let url = v["url"].as_str().filter(|u| u.starts_with("https://") || u.starts_with("http://"));
        let Some(url) = url.map(String::from) else {
            return Err(refused(502, "relay_error", "The relay sent no sign-in address.", "Try again in a moment."));
        };
        let engine = self.clone();
        let task = tokio::spawn(async move {
            let res = engine.account_wait(listener, &state, &verifier).await;
            let mut f = engine.flow();
            f.task = None;
            f.error = res.err().map(|e| match e {
                EngineError::Refused { message, hint, .. } => (message, hint),
                other => (other.to_string(), "Press Sign in to try again.".into()),
            });
        });
        let mut f = self.flow();
        *f = Flow { task: Some(task), started_at: now().0, error: None };
        Ok(json!({ "url": url, "expires_at": v["expires_at"] }))
    }

    /// Stops waiting for the browser (the Cancel link).
    pub fn account_sign_in_cancel(&self) {
        let mut f = self.flow();
        if let Some(t) = f.task.take() {
            t.abort();
        }
        f.error = None;
    }

    async fn account_wait(&self, listener: TcpListener, state: &str, verifier: &str) -> Result<(), EngineError> {
        let code = wait_for_code(listener, state).await?;
        let (_, v) = self
            .pro_call_status(
                reqwest::Method::POST,
                "/v1/account/app/token",
                Some(json!({ "code": code, "code_verifier": verifier })),
            )
            .await?;
        let token = v["session_token"].as_str().filter(|t| !t.is_empty()).ok_or_else(|| {
            refused(502, "relay_error", "The relay didn't send a session.", "Press Sign in to try again.")
        })?;
        self.inner.secrets.set(SESSION, token).map_err(|e| EngineError::Unavailable(e.to_string()))?;
        let email = v.pointer("/account/email").and_then(Value::as_str).unwrap_or("your account").to_string();
        // Pro may now come from the account.
        let _ = self.pro_status(true).await;
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
        match res {
            Ok(_) | Err(EngineError::Refused { code: "signed_out", .. }) => Ok(()),
            // Signed out here; only "everywhere" needs the relay to have heard it.
            Err(e) if everywhere => Err(e),
            Err(_) => Ok(()),
        }
    }

    /// `GET /pro/account/sessions`: the account's signed-in apps and browsers.
    pub async fn account_sessions(&self) -> Result<Value, EngineError> {
        let token = self.account_token()?.ok_or_else(not_signed_in)?;
        Ok(self.account_call(&token, reqwest::Method::GET, "/v1/account/sessions", None).await?.1)
    }

    /// `DELETE /pro/account/sessions/{id}`.
    pub async fn account_revoke(&self, id: &str) -> Result<(), EngineError> {
        if id.is_empty() || id.len() > 80 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(EngineError::Invalid("That session id doesn't look right.".into()));
        }
        let token = self.account_token()?.ok_or_else(not_signed_in)?;
        self.account_call(&token, reqwest::Method::DELETE, &format!("/v1/account/sessions/{id}"), None).await?;
        Ok(())
    }
}

fn not_signed_in() -> EngineError {
    refused(401, "signed_out", "This PC isn't signed in to a Ferrocast account.", "Press Sign in first.")
}

/// Waits for the browser's redirect to `/auth/callback`, checks `state`, returns the code.
async fn wait_for_code(listener: TcpListener, state: &str) -> Result<String, EngineError> {
    let deadline = tokio::time::Instant::now() + SIGN_IN_WAIT;
    loop {
        let (mut socket, _) = tokio::time::timeout_at(deadline, listener.accept())
            .await
            .map_err(|_| refused(410, "flow_expired", "Signing in took too long.", "Press Sign in to try again."))?
            .map_err(|e| EngineError::Unavailable(e.to_string()))?;
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
        if q("state").as_deref() != Some(state) {
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
            Some(code) if !code.is_empty() => {
                let _ = respond(&mut socket, 200, "You're signed in to Ferrocast. You can close this tab.").await;
                return Ok(code);
            }
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
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    );
    socket.write_all(head.as_bytes()).await?;
    socket.write_all(body.as_bytes()).await?;
    socket.shutdown().await
}

#[cfg(test)]
#[path = "ferro_account_tests.rs"]
mod tests;
