//! Facebook sign-in for an installed app: authorization code with a one-time loopback redirect
//! (the system browser, never an embedded webview), then the long-lived token and the Pages
//! the person manages. The app id and secret are the owner's to register with Facebook; the
//! flow only runs when they are configured.

use std::time::Duration;

use base64::Engine as _;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::graph::Page;
use crate::{Endpoints, FacebookError, SCOPES};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppCredentials {
    pub app_id: String,
    pub app_secret: String,
}

/// A sign-in in progress: the URL to open and the loopback listener waiting for Facebook.
#[derive(Debug)]
pub struct Pending {
    pub url: String,
    listener: TcpListener,
    redirect_uri: String,
    state: String,
}

/// What a finished sign-in gives: who signed in, and the Pages they manage (each with its token).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserGrant {
    pub user_id: String,
    pub user_name: String,
    pub pages: Vec<Page>,
}

fn random_b64(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    #[allow(clippy::expect_used)]
    getrandom::fill(&mut b).expect("the OS random number generator is unavailable");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// Opens a loopback listener and builds Facebook's consent URL.
pub async fn begin(ep: &Endpoints, app: &AppCredentials) -> Result<Pending, FacebookError> {
    let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|e| FacebookError::Network(e.to_string()))?;
    let port = listener.local_addr().map_err(|e| FacebookError::Network(e.to_string()))?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let state = random_b64(24);
    let mut url = url::Url::parse(&ep.authorize).map_err(|e| FacebookError::Protocol(e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("client_id", &app.app_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", &SCOPES.join(","))
        .append_pair("state", &state);
    Ok(Pending { url: url.to_string(), listener, redirect_uri, state })
}

async fn respond(socket: &mut tokio::net::TcpStream, status: u16, text: &str) -> std::io::Result<()> {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Ferrocast</title><p style=\"font:16px system-ui;margin:3em\">{text}</p>"
    );
    let head = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    );
    socket.write_all(head.as_bytes()).await?;
    socket.write_all(body.as_bytes()).await?;
    socket.shutdown().await
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

/// Waits (up to `timeout`) for Facebook to redirect back, checks `state`, trades the code for a
/// long-lived token and lists the Pages.
pub async fn finish(
    http: &reqwest::Client,
    ep: &Endpoints,
    app: &AppCredentials,
    pending: Pending,
    timeout: Duration,
) -> Result<UserGrant, FacebookError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let code = loop {
        let (mut socket, _) = tokio::time::timeout_at(deadline, pending.listener.accept())
            .await
            .map_err(|_| FacebookError::SignIn("Signing in to Facebook timed out. Press Connect again.".into()))?
            .map_err(|e| FacebookError::Network(e.to_string()))?;
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
            continue;
        }
        if q("state").as_deref() != Some(pending.state.as_str()) {
            let _ = respond(
                &mut socket,
                400,
                "This sign-in link doesn't match. Go back to Ferrocast and press Connect again.",
            )
            .await;
            return Err(FacebookError::SignIn("The sign-in didn't match (state). Press Connect again.".into()));
        }
        if let Some(err) = q("error") {
            let _ = respond(&mut socket, 200, "Facebook sign-in was cancelled. You can close this tab.").await;
            return Err(FacebookError::SignIn(format!("Facebook sign-in was cancelled ({err}).")));
        }
        match q("code") {
            Some(code) => {
                let _ = respond(&mut socket, 200, "Facebook is connected to Ferrocast. You can close this tab.").await;
                break code;
            }
            None => {
                let _ = respond(&mut socket, 400, "No sign-in code arrived. Press Connect again.").await;
                return Err(FacebookError::SignIn("Facebook didn't send a sign-in code.".into()));
            }
        }
    };
    let token_url = format!("{}/oauth/access_token", ep.graph.trim_end_matches('/'));
    // The code for a short-lived user token, then that for a long-lived one. The secret goes in
    // the request body (a form), never in an address.
    let short: TokenResponse = exchange(
        http,
        &token_url,
        &[
            ("client_id", app.app_id.as_str()),
            ("client_secret", app.app_secret.as_str()),
            ("redirect_uri", pending.redirect_uri.as_str()),
            ("code", code.as_str()),
        ],
    )
    .await?;
    let long: TokenResponse = exchange(
        http,
        &token_url,
        &[
            ("grant_type", "fb_exchange_token"),
            ("client_id", app.app_id.as_str()),
            ("client_secret", app.app_secret.as_str()),
            ("fb_exchange_token", short.access_token.as_str()),
        ],
    )
    .await?;
    let graph = ep.graph.trim_end_matches('/');
    let me: serde_json::Value =
        get_json(http, &format!("{graph}/me"), &long.access_token, &[("fields", "id,name")]).await?;
    let pages_json: serde_json::Value = get_json(
        http,
        &format!("{graph}/me/accounts"),
        &long.access_token,
        &[("fields", "id,name,access_token,picture{url}"), ("limit", "50")],
    )
    .await?;
    let pages = pages_json
        .get("data")
        .and_then(serde_json::Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|p| {
                    Some(Page {
                        id: p.get("id")?.as_str()?.to_string(),
                        name: p.get("name").and_then(serde_json::Value::as_str).unwrap_or("Page").to_string(),
                        access_token: p.get("access_token")?.as_str()?.to_string(),
                        avatar_url: p
                            .pointer("/picture/data/url")
                            .and_then(serde_json::Value::as_str)
                            .map(String::from),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(UserGrant {
        user_id: me.get("id").and_then(serde_json::Value::as_str).unwrap_or_default().to_string(),
        user_name: me.get("name").and_then(serde_json::Value::as_str).unwrap_or("Facebook").to_string(),
        pages,
    })
}

async fn exchange<T: for<'de> Deserialize<'de>>(
    http: &reqwest::Client,
    url: &str,
    form: &[(&str, &str)],
) -> Result<T, FacebookError> {
    let res = http.post(url).form(form).send().await.map_err(FacebookError::from_reqwest)?;
    if !res.status().is_success() {
        return Err(FacebookError::SignIn(format!(
            "Facebook refused the sign-in: {}",
            FacebookError::from_response(res).await
        )));
    }
    res.json::<T>().await.map_err(FacebookError::from_reqwest)
}

async fn get_json(
    http: &reqwest::Client,
    url: &str,
    token: &str,
    query: &[(&str, &str)],
) -> Result<serde_json::Value, FacebookError> {
    let res = http.get(url).bearer_auth(token).query(query).send().await.map_err(FacebookError::from_reqwest)?;
    if !res.status().is_success() {
        return Err(FacebookError::from_response(res).await);
    }
    res.json().await.map_err(FacebookError::from_reqwest)
}
