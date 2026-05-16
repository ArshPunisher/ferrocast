//! Google sign-in for an installed app: authorization code with PKCE and a one-time
//! loopback redirect (R-45: the system browser, never an embedded webview).
//!
//! Google issues installed-app clients a "secret" that it documents as not confidential;
//! release builds inject it at build time so it never sits in this open repo (R-46).

use std::time::Duration;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::{Endpoints, YoutubeError, now_ms};

/// Read and moderate live chat (`youtube.force-ssl` is the scope Google requires for
/// sending and moderating chat), plus reading the channel's own details.
pub const SCOPES: &[&str] =
    &["https://www.googleapis.com/auth/youtube.force-ssl", "https://www.googleapis.com/auth/youtube.readonly"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppCredentials {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
}

impl Tokens {
    pub fn needs_refresh(&self, now: i64) -> bool {
        now >= self.expires_at - 300_000
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
}

/// A sign-in in progress: the URL to open and the loopback listener waiting for Google.
#[derive(Debug)]
pub struct Pending {
    pub url: String,
    pub(crate) listener: TcpListener,
    pub(crate) redirect_uri: String,
    pub(crate) verifier: String,
    pub(crate) state: String,
}

fn random_b64(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    // No OS randomness at all means nothing secure is possible; refuse loudly.
    #[allow(clippy::expect_used)]
    getrandom::fill(&mut b).expect("the OS random number generator is unavailable");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// Opens a loopback listener and builds Google's consent URL.
pub async fn begin(ep: &Endpoints, app: &AppCredentials) -> Result<Pending, YoutubeError> {
    let listener = TcpListener::bind("127.0.0.1:0").await.map_err(YoutubeError::network)?;
    let port = listener.local_addr().map_err(YoutubeError::network)?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let verifier = random_b64(48);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_b64(24);
    let mut url = url::Url::parse(&ep.authorize).map_err(|e| YoutubeError::Protocol(e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("client_id", &app.client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", &SCOPES.join(" "))
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    Ok(Pending { url: url.to_string(), listener, redirect_uri, verifier, state })
}

/// Waits (up to `timeout`) for Google to redirect back, checks `state`, and trades the code.
pub async fn finish(
    http: &reqwest::Client,
    ep: &Endpoints,
    app: &AppCredentials,
    pending: Pending,
    timeout: Duration,
) -> Result<Tokens, YoutubeError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let code = loop {
        let (mut socket, _) = tokio::time::timeout_at(deadline, pending.listener.accept())
            .await
            .map_err(|_| YoutubeError::SignIn("Signing in to YouTube timed out. Press Connect again.".into()))?
            .map_err(YoutubeError::network)?;
        let mut buf = vec![0u8; 8192];
        let n = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buf))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        let target = request.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("");
        let parsed = url::Url::parse(&format!("http://127.0.0.1{target}")).ok();
        let q = |k: &str| {
            parsed.as_ref().and_then(|u| u.query_pairs().find(|(key, _)| key == k).map(|(_, v)| v.into_owned()))
        };
        if parsed.as_ref().map(|u| u.path()) != Some("/callback") {
            let _ = respond(&mut socket, 404, "Not found").await;
            continue; // a browser asking for /favicon.ico
        }
        if q("state").as_deref() != Some(pending.state.as_str()) {
            let _ = respond(
                &mut socket,
                400,
                "This sign-in link doesn't match. Go back to Ferrocast and press Connect again.",
            )
            .await;
            return Err(YoutubeError::SignIn("The sign-in didn't match (state). Press Connect again.".into()));
        }
        if let Some(err) = q("error") {
            let _ = respond(&mut socket, 200, "YouTube sign-in was cancelled. You can close this tab.").await;
            return Err(YoutubeError::SignIn(format!("YouTube sign-in was cancelled ({err}).")));
        }
        match q("code") {
            Some(code) => {
                let _ = respond(&mut socket, 200, "YouTube is connected to Ferrocast. You can close this tab.").await;
                break code;
            }
            None => {
                let _ = respond(&mut socket, 400, "No sign-in code arrived. Press Connect again.").await;
                return Err(YoutubeError::SignIn("Google didn't send a sign-in code.".into()));
            }
        }
    };
    let res = http
        .post(&ep.token)
        .form(&[
            ("code", code.as_str()),
            ("client_id", app.client_id.as_str()),
            ("client_secret", app.client_secret.as_str()),
            ("redirect_uri", pending.redirect_uri.as_str()),
            ("grant_type", "authorization_code"),
            ("code_verifier", pending.verifier.as_str()),
        ])
        .send()
        .await
        .map_err(YoutubeError::network)?;
    if !res.status().is_success() {
        return Err(YoutubeError::SignIn(format!(
            "Google refused the sign-in: {}",
            YoutubeError::from_response(res).await
        )));
    }
    let t: TokenResponse = res.json().await.map_err(YoutubeError::network)?;
    let refresh_token =
        t.refresh_token.ok_or_else(|| YoutubeError::SignIn("Google didn't grant offline access.".into()))?;
    Ok(Tokens {
        access_token: t.access_token,
        refresh_token,
        expires_at: now_ms() + t.expires_in.clamp(0, 86_400) * 1000,
    })
}

/// Google keeps the same refresh token across refreshes unless it sends a new one.
pub async fn refresh(
    http: &reqwest::Client,
    ep: &Endpoints,
    app: &AppCredentials,
    current: &Tokens,
) -> Result<Tokens, YoutubeError> {
    let res = http
        .post(&ep.token)
        .form(&[
            ("client_id", app.client_id.as_str()),
            ("client_secret", app.client_secret.as_str()),
            ("refresh_token", current.refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await
        .map_err(YoutubeError::network)?;
    match res.status().as_u16() {
        200..=299 => {
            let t: TokenResponse = res.json().await.map_err(YoutubeError::network)?;
            Ok(Tokens {
                access_token: t.access_token,
                refresh_token: t.refresh_token.unwrap_or_else(|| current.refresh_token.clone()),
                expires_at: now_ms() + t.expires_in.clamp(0, 86_400) * 1000,
            })
        }
        400 | 401 => Err(YoutubeError::SignInNeeded),
        _ => Err(YoutubeError::from_response(res).await),
    }
}

async fn respond(socket: &mut tokio::net::TcpStream, status: u16, message: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Ferrocast</title><body style=\"font:16px system-ui;background:#0c0e12;color:#e9e9ee;display:grid;place-items:center;height:100vh;margin:0\"><p>{}</p>",
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
