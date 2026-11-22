//! Facebook sign-in through the relay, like Kick's: the Meta app secret lives only here (R-46),
//! never in the desktop binary. The desktop asks for a sign-in address, the person approves in
//! their browser, Meta sends them back to the relay (Meta wants one fixed redirect address, which
//! a desktop's loopback port is not), the relay trades the code and the long-lived token with the
//! secret, and hands the Pages the person manages to that install over its link. The Page tokens
//! the desktop keeps do not expire on their own, so there is nothing to refresh here.

use std::fmt;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ApiError, AppState, authed, page, random};

/// What the Page-facing app is allowed to ask for (mirrors `ferrocast_facebook::SCOPES`).
pub const SCOPES: &[&str] = &[
    "pages_show_list",
    "pages_read_engagement",
    "pages_manage_posts",
    "pages_manage_engagement",
    "pages_read_user_content",
    "publish_video",
];

/// The app's id and secret, and the addresses of Meta's login dialog and Graph API (tests point
/// them at a fake).
#[derive(Clone, Default)]
pub struct Config {
    pub app_id: Option<String>,
    pub app_secret: Option<String>,
    pub dialog_url: String,
    pub graph_url: String,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("app_id", &self.app_id)
            .field("app_secret", &self.app_secret.as_ref().map(|_| "[REDACTED]"))
            .field("dialog_url", &self.dialog_url)
            .field("graph_url", &self.graph_url)
            .finish()
    }
}

impl Config {
    /// `FACEBOOK_APP_ID` and `FACEBOOK_APP_SECRET`; off without both.
    pub fn from_env() -> Config {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Config {
            app_id: env("FACEBOOK_APP_ID"),
            app_secret: env("FACEBOOK_APP_SECRET"),
            dialog_url: env("FACEBOOK_DIALOG_URL")
                .unwrap_or_else(|| "https://www.facebook.com/v21.0/dialog/oauth".into()),
            graph_url: env("FACEBOOK_GRAPH_URL").unwrap_or_else(|| "https://graph.facebook.com/v21.0".into()),
        }
    }

    fn app(&self) -> Option<(&str, &str)> {
        Some((self.app_id.as_deref()?, self.app_secret.as_deref()?))
    }
}

#[derive(Debug, Default)]
pub(crate) struct Runtime {
    pub(crate) cfg: Config,
}

impl Runtime {
    pub(crate) fn new(cfg: Config) -> Runtime {
        Runtime { cfg }
    }
}

fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "facebook_unavailable",
        "Facebook sign-in isn't set up on this relay yet.",
    )
}

/// Starts Facebook sign-in for an install: returns the address to open in the browser.
pub(crate) async fn oauth(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let install = authed(&st, &headers)?;
    let cfg = &st.facebook.cfg;
    let (app_id, _) = cfg.app().ok_or_else(unavailable)?;
    let state = random(24);
    // Facebook Login has no PKCE for this flow: the code is useless without the secret, which
    // only the relay holds, and the state ties the answer to the install that asked.
    st.db.put_oauth(&state, &install, "-").map_err(ApiError::db)?;
    let mut url = url::Url::parse(&cfg.dialog_url).map_err(|_| {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "config", "The relay's Facebook address is wrong.")
    })?;
    url.query_pairs_mut()
        .append_pair("client_id", app_id)
        .append_pair("redirect_uri", &format!("{}/v1/facebook/callback", st.cfg.public_url))
        .append_pair("response_type", "code")
        .append_pair("scope", &SCOPES.join(","))
        .append_pair("state", &state);
    Ok(axum::Json(json!({ "url": url.to_string() })).into_response())
}

#[derive(Deserialize)]
pub(crate) struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// One call to Meta's token endpoint (the secret goes in the body, never in an address).
async fn token(st: &AppState, form: &[(&str, &str)]) -> Option<String> {
    let res = st.http.post(format!("{}/oauth/access_token", st.facebook.cfg.graph_url)).form(form).send().await.ok()?;
    if !res.status().is_success() {
        return None;
    }
    let v: Value = res.json().await.ok()?;
    v.get("access_token").and_then(Value::as_str).filter(|t| !t.is_empty()).map(String::from)
}

async fn graph_get(st: &AppState, path: &str, token: &str) -> Option<Value> {
    let res = st.http.get(format!("{}{path}", st.facebook.cfg.graph_url)).bearer_auth(token).send().await.ok()?;
    res.status().is_success().then_some(())?;
    res.json().await.ok()
}

/// Meta sends the person back here. Trades the code (with the secret), then the long-lived token,
/// learns who they are and which Pages they manage, and hands that to the install's desktop.
pub(crate) async fn callback(State(st): State<AppState>, Query(q): Query<Callback>) -> Response {
    if q.error.is_some() {
        return page(StatusCode::OK, "Facebook sign-in was cancelled. You can close this tab.");
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return page(
            StatusCode::BAD_REQUEST,
            "This link is incomplete. Go back to Ferrocast and press Connect Facebook again.",
        );
    };
    let Ok(Some((install, _))) = st.db.take_oauth(&state) else {
        return page(
            StatusCode::BAD_REQUEST,
            "This sign-in link expired or was already used. Press Connect Facebook again in Ferrocast.",
        );
    };
    let Some((app_id, secret)) = st.facebook.cfg.app() else {
        return page(StatusCode::SERVICE_UNAVAILABLE, "Facebook sign-in isn't set up on this relay yet.");
    };
    let redirect = format!("{}/v1/facebook/callback", st.cfg.public_url);
    let Some(short) =
        token(&st, &[("client_id", app_id), ("client_secret", secret), ("redirect_uri", &redirect), ("code", &code)])
            .await
    else {
        return page(
            StatusCode::BAD_GATEWAY,
            "Facebook didn't accept the sign-in. Press Connect Facebook again in Ferrocast.",
        );
    };
    let Some(long) = token(
        &st,
        &[
            ("grant_type", "fb_exchange_token"),
            ("client_id", app_id),
            ("client_secret", secret),
            ("fb_exchange_token", &short),
        ],
    )
    .await
    else {
        return page(StatusCode::BAD_GATEWAY, "Facebook didn't finish the sign-in. Press Connect Facebook again.");
    };
    let Some(me) = graph_get(&st, "/me?fields=id,name", &long).await else {
        return page(StatusCode::BAD_GATEWAY, "Facebook didn't say whose account this is. Try again.");
    };
    let (Some(user_id), name) = (
        me.get("id").and_then(Value::as_str).map(String::from),
        me.get("name").and_then(Value::as_str).unwrap_or("Facebook").to_string(),
    ) else {
        return page(StatusCode::BAD_GATEWAY, "Facebook sent an account without an id.");
    };
    let Some(accounts) =
        graph_get(&st, "/me/accounts?fields=id,name,access_token,picture%7Burl%7D&limit=25", &long).await
    else {
        return page(StatusCode::BAD_GATEWAY, "Facebook didn't list your Pages. Try again.");
    };
    // Only a Page that came with its own token can act as the Page.
    let pages: Vec<Value> = accounts
        .get("data")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|p| {
                    let id = p.get("id").and_then(Value::as_str)?;
                    let access = p.get("access_token").and_then(Value::as_str).filter(|t| !t.is_empty())?;
                    let avatar =
                        p.pointer("/picture/data/url").and_then(Value::as_str).filter(|u| u.starts_with("https://"));
                    Some(json!({
                        "id": id,
                        "name": p.get("name").and_then(Value::as_str).unwrap_or("Page"),
                        "access_token": access,
                        "avatar_url": avatar,
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    let payload = json!({ "user": { "id": user_id, "name": name }, "pages": pages });
    if st.enqueue(&install, "facebook_connected", &payload).is_err() {
        return page(StatusCode::SERVICE_UNAVAILABLE, "The relay couldn't save that. Try again shortly.");
    }
    page(StatusCode::OK, "Facebook is connected to Ferrocast. You can close this tab.")
}
