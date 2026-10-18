//! Discord's REST API, behind a trait so the relay's tests run against a fake (no network).
//! REST only: no gateway connection, so the relay holds no socket per server.
//!
//! The bot token and the OAuth client secret never appear in `Debug`, an error or a log.
//! Viewer OAuth tokens are used for two reads and revoked right away (never stored).

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

pub const DEFAULT_BASE: &str = "https://discord.com/api/v10";
const TIMEOUT: Duration = Duration::from_secs(8);
const USER_AGENT: &str = "DiscordBot (https://ferrocast.clearcraftlabs.com, 1)";

/// Discord's JSON error codes the relay acts on.
pub mod codes {
    pub const UNKNOWN_CHANNEL: i64 = 10003;
    pub const UNKNOWN_GUILD: i64 = 10004;
    pub const UNKNOWN_MEMBER: i64 = 10007;
    pub const UNKNOWN_ROLE: i64 = 10011;
    /// 403 Missing Access: the bot can't see the channel, or isn't in the server any more.
    pub const MISSING_ACCESS: i64 = 50001;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Guild {
    pub id: String,
    pub name: String,
    /// The icon hash (`icons/<id>/<hash>.png`), if it has one.
    pub icon: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    pub id: String,
    pub name: String,
    /// 0 text, 5 announcement, others (voice, category, forum) aren't offered.
    pub kind: i64,
    pub position: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    pub id: String,
    pub name: String,
    pub color: i64,
    pub position: i64,
    /// Belongs to an integration or a bot: nobody can hand it out.
    pub managed: bool,
}

/// One account a viewer connected to Discord (User Settings › Connections).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    /// `twitch`, `youtube`, … (Discord's own names).
    pub kind: String,
    /// The platform's user id (Twitch user id, YouTube channel id).
    pub id: String,
    /// Shown to the viewer on the result page, never stored.
    pub name: String,
    pub verified: bool,
}

/// An OAuth code exchanged: the token (used at once, then revoked) and, for a bot install,
/// the server it was added to. Discord says which server here, so a crafted `guild_id` in the
/// callback URL can't claim someone else's server.
#[derive(Clone, PartialEq, Eq)]
pub struct Token {
    pub access_token: String,
    pub guild: Option<Guild>,
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token").field("access_token", &"<redacted>").field("guild", &self.guild).finish()
    }
}

/// Why Discord said no.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiscordError {
    /// 50013 (or any 403 but 50001): the bot's role is below the role, or a permission is off.
    #[error("Discord says Ferrocast is missing a permission")]
    MissingPermission,
    /// 50001 Missing Access: the bot can't see the channel, or was removed from the server.
    #[error("Discord says Ferrocast has no access")]
    MissingAccess,
    /// 429: wait this many seconds.
    #[error("Discord is rate limiting (retry after {retry_after_s} s)")]
    RateLimited { retry_after_s: u64 },
    /// 404 with Discord's code (`codes::UNKNOWN_*`).
    #[error("Discord doesn't know that ({0})")]
    NotFound(i64),
    /// 401: the bot token (or an OAuth code) is bad.
    #[error("Discord refused the sign-in")]
    Unauthorized,
    /// 400: Discord didn't like the request (an expired OAuth code, a bad embed).
    #[error("Discord refused the request: {0}")]
    BadRequest(String),
    /// Down, slow, or an answer we can't read.
    #[error("Discord is unavailable: {0}")]
    Unavailable(String),
}

pub type Fut<'a, T> = Pin<Box<dyn Future<Output = Result<T, DiscordError>> + Send + 'a>>;

/// The calls Ferrocast makes. `HttpDiscord` in production, a fake in tests.
pub trait DiscordApi: Send + Sync + std::fmt::Debug {
    /// Trades an OAuth code (bot install or viewer link) for a token.
    fn exchange_code<'a>(&'a self, code: &'a str, redirect_uri: &'a str) -> Fut<'a, Token>;
    /// Revokes a token we're done with.
    fn revoke<'a>(&'a self, access_token: &'a str) -> Fut<'a, ()>;
    /// The token's Discord user id.
    fn me<'a>(&'a self, access_token: &'a str) -> Fut<'a, String>;
    /// The token user's connected accounts.
    fn connections<'a>(&'a self, access_token: &'a str) -> Fut<'a, Vec<Connection>>;
    fn guild<'a>(&'a self, guild: &'a str) -> Fut<'a, Guild>;
    fn channels<'a>(&'a self, guild: &'a str) -> Fut<'a, Vec<Channel>>;
    fn roles<'a>(&'a self, guild: &'a str) -> Fut<'a, Vec<Role>>;
    /// The role ids Ferrocast's own bot member has in the server.
    fn bot_roles<'a>(&'a self, guild: &'a str) -> Fut<'a, Vec<String>>;
    fn add_role<'a>(&'a self, guild: &'a str, user: &'a str, role: &'a str) -> Fut<'a, ()>;
    fn remove_role<'a>(&'a self, guild: &'a str, user: &'a str, role: &'a str) -> Fut<'a, ()>;
    /// Posts a message; its id back.
    fn post<'a>(&'a self, channel: &'a str, body: Value) -> Fut<'a, String>;
    fn edit<'a>(&'a self, channel: &'a str, message: &'a str, body: Value) -> Fut<'a, ()>;
    /// The bot leaves the server.
    fn leave<'a>(&'a self, guild: &'a str) -> Fut<'a, ()>;
}

/// The real thing, over HTTPS.
#[derive(Clone)]
pub struct HttpDiscord {
    http: reqwest::Client,
    base: String,
    app_id: String,
    bot_token: String,
    client_secret: String,
}

impl std::fmt::Debug for HttpDiscord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpDiscord")
            .field("base", &self.base)
            .field("app_id", &self.app_id)
            .field("bot_token", &"<redacted>")
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

impl HttpDiscord {
    pub fn new(base: String, app_id: String, bot_token: String, client_secret: String) -> HttpDiscord {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .connect_timeout(Duration::from_secs(4))
            .user_agent(USER_AGENT)
            .build()
            .unwrap_or_default();
        HttpDiscord { http, base: base.trim_end_matches('/').to_string(), app_id, bot_token, client_secret }
    }

    fn bot(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.base))
            .header(reqwest::header::AUTHORIZATION, format!("Bot {}", self.bot_token))
    }

    fn user(&self, path: &str, token: &str) -> reqwest::RequestBuilder {
        self.http.get(format!("{}{path}", self.base)).bearer_auth(token)
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Value, DiscordError> {
        let res = req.send().await.map_err(|e| {
            tracing::warn!(error = %e.without_url(), "couldn't reach Discord");
            DiscordError::Unavailable("unreachable".into())
        })?;
        let status = res.status().as_u16();
        let header_wait =
            res.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<f64>().ok());
        let bytes = res.bytes().await.map_err(|_| DiscordError::Unavailable("the answer broke off".into()))?;
        let v: Value =
            if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) };
        if (200..300).contains(&status) {
            return Ok(v);
        }
        Err(classify(status, &v, header_wait))
    }
}

/// Discord's error answer → what the relay can do about it.
pub(crate) fn classify(status: u16, v: &Value, header_wait: Option<f64>) -> DiscordError {
    let code = v.get("code").and_then(Value::as_i64).unwrap_or(0);
    match status {
        429 => {
            let wait = v.get("retry_after").and_then(Value::as_f64).or(header_wait).unwrap_or(5.0);
            DiscordError::RateLimited { retry_after_s: (wait.max(0.0).ceil() as u64).clamp(1, 3_600) }
        }
        // 50001 Missing Access (a hidden channel, or the bot was removed from the server).
        403 if code == codes::MISSING_ACCESS => DiscordError::MissingAccess,
        // 50013 Missing Permissions (the role order) and any other 403.
        403 => DiscordError::MissingPermission,
        404 => DiscordError::NotFound(code),
        401 => DiscordError::Unauthorized,
        400 => {
            // Discord's message only (never our request back): "Invalid \"code\" in request."
            let m = v
                .get("error_description")
                .or_else(|| v.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("bad request");
            DiscordError::BadRequest(m.chars().take(160).collect())
        }
        s => DiscordError::Unavailable(format!("Discord answered {s}")),
    }
}

fn s(v: &Value, k: &str) -> String {
    match &v[k] {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn bad_answer() -> DiscordError {
    DiscordError::Unavailable("an answer Ferrocast can't read".into())
}

pub(crate) fn guild_from(v: &Value) -> Option<Guild> {
    let id = s(v, "id");
    (!id.is_empty()).then(|| Guild { id, name: s(v, "name"), icon: v["icon"].as_str().map(String::from) })
}

impl DiscordApi for HttpDiscord {
    fn exchange_code<'a>(&'a self, code: &'a str, redirect_uri: &'a str) -> Fut<'a, Token> {
        Box::pin(async move {
            let form = [("grant_type", "authorization_code"), ("code", code), ("redirect_uri", redirect_uri)];
            let req = self
                .http
                .post(format!("{}/oauth2/token", self.base))
                .basic_auth(&self.app_id, Some(&self.client_secret))
                .form(&form);
            let v = self.send(req).await?;
            let access_token = v["access_token"].as_str().ok_or_else(bad_answer)?.to_string();
            Ok(Token { access_token, guild: v.get("guild").and_then(guild_from) })
        })
    }

    fn revoke<'a>(&'a self, access_token: &'a str) -> Fut<'a, ()> {
        Box::pin(async move {
            let form = [("token", access_token), ("token_type_hint", "access_token")];
            let req = self
                .http
                .post(format!("{}/oauth2/token/revoke", self.base))
                .basic_auth(&self.app_id, Some(&self.client_secret))
                .form(&form);
            self.send(req).await.map(|_| ())
        })
    }

    fn me<'a>(&'a self, access_token: &'a str) -> Fut<'a, String> {
        Box::pin(async move {
            let v = self.send(self.user("/users/@me", access_token)).await?;
            let id = s(&v, "id");
            if id.is_empty() { Err(bad_answer()) } else { Ok(id) }
        })
    }

    fn connections<'a>(&'a self, access_token: &'a str) -> Fut<'a, Vec<Connection>> {
        Box::pin(async move {
            let v = self.send(self.user("/users/@me/connections", access_token)).await?;
            let list = v.as_array().ok_or_else(bad_answer)?;
            Ok(list
                .iter()
                .map(|c| Connection {
                    kind: s(c, "type"),
                    id: s(c, "id"),
                    name: s(c, "name"),
                    verified: c["verified"].as_bool().unwrap_or(false),
                })
                .collect())
        })
    }

    fn guild<'a>(&'a self, guild: &'a str) -> Fut<'a, Guild> {
        Box::pin(async move {
            let v = self.send(self.bot(reqwest::Method::GET, &format!("/guilds/{guild}"))).await?;
            guild_from(&v).ok_or_else(bad_answer)
        })
    }

    fn channels<'a>(&'a self, guild: &'a str) -> Fut<'a, Vec<Channel>> {
        Box::pin(async move {
            let v = self.send(self.bot(reqwest::Method::GET, &format!("/guilds/{guild}/channels"))).await?;
            let list = v.as_array().ok_or_else(bad_answer)?;
            Ok(list
                .iter()
                .map(|c| Channel {
                    id: s(c, "id"),
                    name: s(c, "name"),
                    kind: c["type"].as_i64().unwrap_or(-1),
                    position: c["position"].as_i64().unwrap_or(0),
                })
                .collect())
        })
    }

    fn roles<'a>(&'a self, guild: &'a str) -> Fut<'a, Vec<Role>> {
        Box::pin(async move {
            let v = self.send(self.bot(reqwest::Method::GET, &format!("/guilds/{guild}/roles"))).await?;
            let list = v.as_array().ok_or_else(bad_answer)?;
            Ok(list
                .iter()
                .map(|r| Role {
                    id: s(r, "id"),
                    name: s(r, "name"),
                    color: r["color"].as_i64().unwrap_or(0),
                    position: r["position"].as_i64().unwrap_or(0),
                    managed: r["managed"].as_bool().unwrap_or(false),
                })
                .collect())
        })
    }

    fn bot_roles<'a>(&'a self, guild: &'a str) -> Fut<'a, Vec<String>> {
        Box::pin(async move {
            let path = format!("/guilds/{guild}/members/{}", self.app_id);
            let v = self.send(self.bot(reqwest::Method::GET, &path)).await?;
            Ok(v["roles"]
                .as_array()
                .map(|a| a.iter().filter_map(|r| r.as_str().map(String::from)).collect())
                .unwrap_or_default())
        })
    }

    fn add_role<'a>(&'a self, guild: &'a str, user: &'a str, role: &'a str) -> Fut<'a, ()> {
        Box::pin(async move {
            let path = format!("/guilds/{guild}/members/{user}/roles/{role}");
            self.send(self.bot(reqwest::Method::PUT, &path).header("x-audit-log-reason", "Ferrocast perk"))
                .await
                .map(|_| ())
        })
    }

    fn remove_role<'a>(&'a self, guild: &'a str, user: &'a str, role: &'a str) -> Fut<'a, ()> {
        Box::pin(async move {
            let path = format!("/guilds/{guild}/members/{user}/roles/{role}");
            self.send(self.bot(reqwest::Method::DELETE, &path).header("x-audit-log-reason", "Ferrocast perk ended"))
                .await
                .map(|_| ())
        })
    }

    fn post<'a>(&'a self, channel: &'a str, body: Value) -> Fut<'a, String> {
        Box::pin(async move {
            let v = self
                .send(self.bot(reqwest::Method::POST, &format!("/channels/{channel}/messages")).json(&body))
                .await?;
            let id = s(&v, "id");
            if id.is_empty() { Err(bad_answer()) } else { Ok(id) }
        })
    }

    fn edit<'a>(&'a self, channel: &'a str, message: &'a str, body: Value) -> Fut<'a, ()> {
        Box::pin(async move {
            let path = format!("/channels/{channel}/messages/{message}");
            self.send(self.bot(reqwest::Method::PATCH, &path).json(&body)).await.map(|_| ())
        })
    }

    fn leave<'a>(&'a self, guild: &'a str) -> Fut<'a, ()> {
        Box::pin(async move {
            self.send(self.bot(reqwest::Method::DELETE, &format!("/users/@me/guilds/{guild}"))).await.map(|_| ())
        })
    }
}
