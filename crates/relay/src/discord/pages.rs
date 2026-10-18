//! The bot's small public pages: Discord's OAuth callback (bot install and viewer link) and the
//! viewer link page. Plain HTML: no scripts, nothing from other sites, every value escaped,
//! `no-store`, `no-referrer`, a CSP that allows only inline styles.
//!
//! Viewer linking is started by the viewer (never posted by us): `/d/<code>/link` explains
//! what's kept, its button goes to Discord's sign-in with `identify connections`, and the
//! callback reads the viewer's id and connected Twitch / YouTube accounts, stores only the ids,
//! and revokes the token. Kick isn't one of Discord's connections: Kick linking is a later
//! `/ferrocast link` slash command.

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use super::api::DiscordError;
use super::{MAX_LINKS, Purpose, check_state, redirect_uri, sign_state};
use crate::{AppState, now_ms};

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// `body` is already-escaped HTML.
fn shell(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <meta name=robots content=noindex><title>{t}</title>\
         <style>:root{{color-scheme:dark light;--bg:#0c0e12;--card:#15181f;--line:#2a2f3a;--text:#ecedf1;--muted:#a3a8b5;--accent:#5865f2}}\
         @media (prefers-color-scheme:light){{:root{{--bg:#f6f6f4;--card:#fff;--line:#dedfe3;--text:#14161b;--muted:#5b606c}}}}\
         body{{margin:0;min-height:100vh;display:grid;place-items:center;padding:16px;box-sizing:border-box;background:var(--bg);color:var(--text);font:17px/1.5 system-ui,sans-serif}}\
         main{{width:100%;max-width:480px;background:var(--card);border:1px solid var(--line);border-radius:16px;padding:28px;box-sizing:border-box}}\
         h1{{font-size:1.5rem;line-height:1.25;margin:0 0 12px;overflow-wrap:anywhere}}p{{margin:0 0 12px}}.muted{{color:var(--muted);font-size:.92rem}}\
         ul{{margin:0 0 16px;padding-left:20px}}li{{overflow-wrap:anywhere}}\
         .btn{{display:inline-flex;align-items:center;justify-content:center;min-height:48px;padding:0 22px;margin:8px 0 16px;border-radius:12px;background:var(--accent);color:#fff;font-weight:700;text-decoration:none}}\
         .btn:focus-visible{{outline:2px solid var(--text);outline-offset:3px}}</style>\
         <main><h1>{t}</h1>{body}</main></html>",
        t = esc(title),
    );
    let mut r = (status, Html(html)).into_response();
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; form-action 'none'; frame-ancestors 'none'; base-uri 'none'"),
    );
    r
}

fn message(status: StatusCode, title: &str, text: &str) -> Response {
    shell(status, title, &format!("<p>{}</p>", esc(text)))
}

fn inactive() -> Response {
    message(StatusCode::NOT_FOUND, "This link isn't active", "Ask the streamer for a new link.")
}

/// A link code is what `random(8)` makes: URL-safe base64.
fn good_code(code: &str) -> bool {
    (8..=32).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The server behind a link code, if its subscription has Pro now.
fn linkable(st: &AppState, code: &str) -> Option<super::store::GuildRow> {
    if !good_code(code) {
        return None;
    }
    let g = st.db.discord_guild_by_code(code).ok().flatten()?;
    let sub = st.db.pro_sub(&g.sub_id).ok().flatten()?;
    crate::billing::access::is_pro(&sub.access(), now_ms()).then_some(g)
}

/// `GET /d/<code>/link`: what linking does, and the button to Discord.
pub(crate) async fn link_page(State(st): State<AppState>, Path(code): Path<String>) -> Response {
    if st.discord.api().is_err() {
        return message(StatusCode::SERVICE_UNAVAILABLE, "Not available", "Discord linking isn't set up here yet.");
    }
    let Some(g) = linkable(&st, &code) else { return inactive() };
    let body = format!(
        "<p>Link your Discord to the Twitch or YouTube account you watch with. Roles for subs, memberships and tips then reach you in <b>{name}</b>.</p>\
         <p class=muted>Connect Twitch or YouTube in Discord first: User Settings › Connections.</p>\
         <a class=btn href=\"/d/{code}/link/start\" rel=\"nofollow noreferrer\">Link with Discord</a>\
         <p class=muted>Kept: your Discord user id and the ids of the Twitch and YouTube accounts connected to it. Not kept: names, messages, your email or the sign-in itself (it's thrown away at once).</p>",
        name = esc(&g.name),
        code = esc(&code),
    );
    shell(StatusCode::OK, "Get your perks on Discord", &body)
}

/// `GET /d/<code>/link/start` → 303 to Discord's sign-in.
pub(crate) async fn link_start(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Path(code): Path<String>,
) -> Response {
    let Ok(cfg) = st.discord.cfg() else {
        return message(StatusCode::SERVICE_UNAVAILABLE, "Not available", "Discord linking isn't set up here yet.");
    };
    if !st.discord.take_ip(crate::client_ip(addr, &headers), now_ms()) {
        return busy();
    }
    if linkable(&st, &code).is_none() {
        return inactive();
    }
    let Ok(mut url) = url::Url::parse("https://discord.com/oauth2/authorize") else { return inactive() };
    url.query_pairs_mut()
        .append_pair("client_id", &cfg.app_id)
        .append_pair("response_type", "code")
        .append_pair("scope", "identify connections")
        .append_pair("redirect_uri", &redirect_uri(&st))
        .append_pair("prompt", "consent")
        .append_pair("state", &sign_state(cfg, Purpose::Link, &code, now_ms()));
    let mut r = StatusCode::SEE_OTHER.into_response();
    if let Ok(v) = HeaderValue::from_str(url.as_str()) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r.headers_mut().insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    r
}

fn busy() -> Response {
    let mut r = message(StatusCode::TOO_MANY_REQUESTS, "Too many tries", "Wait a few minutes, then try again.");
    r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("600"));
    r
}

#[derive(Deserialize)]
pub(crate) struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// `GET /discord/callback`: Discord sends the streamer (bot install) or a viewer (link) back here.
pub(crate) async fn callback(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let Ok(cfg) = st.discord.cfg() else {
        return message(StatusCode::SERVICE_UNAVAILABLE, "Not available", "The Ferrocast bot isn't set up here yet.");
    };
    let now = now_ms();
    let Some((purpose, subject)) = q.state.as_deref().and_then(|s| check_state(cfg, s, now)) else {
        return message(
            StatusCode::BAD_REQUEST,
            "This link expired",
            "It's older than 10 minutes or was changed. Start again from where you came from.",
        );
    };
    if q.error.is_some() {
        return message(StatusCode::OK, "Cancelled", "Nothing changed. You can close this tab.");
    }
    let Some(code) = q.code.filter(|c| !c.is_empty() && c.len() <= 200) else {
        return message(
            StatusCode::BAD_REQUEST,
            "Something's missing",
            "Discord didn't send a sign-in code. Try again.",
        );
    };
    match purpose {
        Purpose::Install => install_done(&st, &subject, &code).await,
        Purpose::Link => {
            if !st.discord.take_ip(crate::client_ip(addr, &headers), now) {
                return busy();
            }
            link_done(&st, &subject, &code).await
        }
    }
}

fn discord_trouble(e: &DiscordError) -> Response {
    match e {
        DiscordError::BadRequest(_) | DiscordError::Unauthorized => {
            message(StatusCode::BAD_REQUEST, "That sign-in didn't work", "It may have expired. Start again.")
        }
        DiscordError::RateLimited { .. } => busy(),
        _ => message(StatusCode::BAD_GATEWAY, "Discord didn't answer", "Try again in a minute."),
    }
}

/// The bot was added: Discord's token answer says to which server. Discord has already put
/// the bot in that server, so whenever it isn't kept (Pro isn't active, this Pro has another
/// server) it leaves again; never for a server another Pro has (that one is theirs).
async fn install_done(st: &AppState, install: &str, code: &str) -> Response {
    let Ok(api) = st.discord.api() else { return inactive() };
    let Ok(cfg) = st.discord.cfg() else { return inactive() };
    let token = match api.exchange_code(code, &redirect_uri(st)).await {
        Ok(t) => t,
        Err(e) => return discord_trouble(&e),
    };
    // We only needed to know the server: the token goes at once.
    if let Err(e) = api.revoke(&token.access_token).await {
        tracing::debug!(error = %e, "couldn't revoke a bot install token (it expires by itself)");
    }
    let Some(guild) = token.guild else {
        return message(
            StatusCode::BAD_REQUEST,
            "No server picked",
            "Discord didn't say which server Ferrocast joined. Press Connect server again.",
        );
    };
    let sub = match st.db.pro_sub_for_install(install) {
        Ok(Some(s)) if crate::billing::access::is_pro(&s.access(), now_ms()) => s,
        Ok(_) => {
            unless_owned(st, &guild.id).await;
            return message(
                StatusCode::PAYMENT_REQUIRED,
                "Pro isn't active",
                "Ferrocast Pro isn't active on that PC any more, so the bot wasn't connected and left the server.",
            );
        }
        Err(_) => {
            return message(
                StatusCode::SERVICE_UNAVAILABLE,
                "Try again",
                "The relay couldn't check that. Try again shortly.",
            );
        }
    };
    let guild = super::api::Guild { name: super::clean(&guild.name, 100), ..guild };
    match st.db.discord_claim(&sub.id, install, &guild, &crate::random(8), now_ms()) {
        Ok(super::store::Claimed::Yes) => {
            st.discord.forget_snapshot(&guild.id);
            shell(
                StatusCode::OK,
                "Ferrocast is in your server",
                &format!(
                    "<p>Connected to <b>{}</b> for the Ferrocast PC with the code <b translate=no>{}</b>.</p>\
                     <p class=muted>Settings › Discord on your PC shows the same code. If it doesn't, or you didn't start this from your own Ferrocast, remove the bot in Server Settings › Integrations.</p>\
                     <p class=muted>For role perks, keep Ferrocast's role above the roles it gives (Server Settings › Roles). You can close this tab.</p>",
                    esc(&guild.name),
                    esc(&super::pc_code(cfg, install)),
                ),
            )
        }
        Ok(super::store::Claimed::Taken) => message(
            StatusCode::CONFLICT,
            "Already connected elsewhere",
            "That server is connected to another Ferrocast Pro. Disconnect it there first.",
        ),
        Ok(super::store::Claimed::HasOther) => {
            super::leave(st, &guild.id).await;
            message(
                StatusCode::CONFLICT,
                "One server per Pro",
                "Another server is already connected, so the bot left this one. Disconnect the other in Settings › Discord first.",
            )
        }
        Err(e) => {
            tracing::warn!(error = %e, "couldn't save a Discord server");
            message(StatusCode::SERVICE_UNAVAILABLE, "Try again", "The relay couldn't save that. Try again shortly.")
        }
    }
}

/// Leaves a server the bot was just added to, unless a subscription has it.
async fn unless_owned(st: &AppState, guild: &str) {
    match st.db.discord_guild_owner(guild) {
        Ok(None) => super::leave(st, guild).await,
        Ok(Some(_)) => {}
        Err(e) => tracing::warn!(error = %e, "couldn't check who has a Discord server; the bot stays"),
    }
}

/// A viewer signed in: keep the ids of their Twitch / YouTube connections, drop the token.
async fn link_done(st: &AppState, link_code: &str, code: &str) -> Response {
    let Ok(api) = st.discord.api() else { return inactive() };
    let Some(g) = linkable(st, link_code) else { return inactive() };
    let token = match api.exchange_code(code, &redirect_uri(st)).await {
        Ok(t) => t,
        Err(e) => return discord_trouble(&e),
    };
    let read = async {
        let user = api.me(&token.access_token).await?;
        let conns = api.connections(&token.access_token).await?;
        Ok::<_, DiscordError>((user, conns))
    }
    .await;
    // Always revoked, whatever happened: the relay never keeps a viewer's sign-in.
    if let Err(e) = api.revoke(&token.access_token).await {
        tracing::debug!(error = %e, "couldn't revoke a viewer token (it expires by itself)");
    }
    drop(token);
    let (user, conns) = match read {
        Ok(x) => x,
        Err(e) => return discord_trouble(&e),
    };
    let mut accounts: Vec<(String, String)> = Vec::new();
    let mut shown: Vec<String> = Vec::new();
    for c in conns.iter().filter(|c| c.verified && (c.kind == "twitch" || c.kind == "youtube")) {
        let valid = !c.id.is_empty()
            && c.id.len() <= 64
            && c.id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if valid && !accounts.iter().any(|(k, _)| k == &c.kind) {
            accounts.push((c.kind.clone(), c.id.clone()));
            let label = if c.kind == "twitch" { "Twitch" } else { "YouTube" };
            shown.push(format!("<li>{label}: {}</li>", esc(&super::clean(&c.name, 80))));
        }
    }
    match st.db.discord_link(&g.guild_id, &user, &accounts, MAX_LINKS, now_ms()) {
        Ok(true) => {}
        Ok(false) => {
            return message(StatusCode::CONFLICT, "The list is full", "This server can't link more members right now.");
        }
        Err(e) => {
            tracing::warn!(error = %e, "couldn't save a Discord link");
            return message(StatusCode::SERVICE_UNAVAILABLE, "Try again", "That didn't save. Try again shortly.");
        }
    }
    if accounts.is_empty() {
        return message(
            StatusCode::OK,
            "No Twitch or YouTube account found",
            "Connect one in Discord (User Settings › Connections), then open the link again.",
        );
    }
    shell(
        StatusCode::OK,
        "You're linked",
        &format!(
            "<ul>{}</ul><p>Roles arrive in <b>{}</b> with your next sub, membership or tip.</p>\
             <p class=muted>To unlink, remove the connection in Discord and open the link again.</p>",
            shown.join(""),
            esc(&g.name)
        ),
    )
}
