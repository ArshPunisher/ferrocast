//! Moderation by the cloud bot while the streamer's PC is off (Pro F2's offline part,
//! milestone M10; API.md §9.5): a phone's ban, timeout or delete goes out with the bot
//! account's own sign-in, which carries the moderator scopes only after the opt-in ("Let the
//! cloud bot moderate when this PC is off", `connect {moderation: true}`). Without them: 403
//! `needs_moderation_scope`. Each action counts in the channel's 20 a minute, like a reply.
//!
//! Twitch: Ban User / Delete Chat Messages with the bot account's user token
//! (`moderator:manage:banned_users`, `moderator:manage:chat_messages`; the bot must be a
//! moderator there, or the streamer's own account). Kick: its moderation API with the
//! sign-in's `moderation:ban` / `moderation:chat_message:manage`. Every call sits behind the
//! platform traits (fakes in tests).

use axum::http::{HeaderName, StatusCode, header};
use ferrocast_core::event::Platform;

use super::kick::KickError;
use super::twitch::{ModTarget, TwitchError};
use super::{MESSAGES_PER_MINUTE, SendErr, key, store};
use crate::quota::{Period, TakeErr};
use crate::{ApiError, AppState, now_ms};

/// The longest timeout Twitch takes (14 days); Kick takes up to 7 days.
pub const MAX_TIMEOUT_S: u32 = 1_209_600;
pub const KICK_MAX_TIMEOUT_S: u32 = 604_800;
/// How long a refreshed Twitch user token is used before it's refreshed again (Twitch's last
/// about four hours).
const TWITCH_USER_TTL_MS: i64 = 60 * 60_000;

/// One moderation action from a phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action<'a> {
    Delete { message_id: &'a str },
    Timeout { user_id: &'a str, seconds: u32, reason: Option<&'a str> },
    Ban { user_id: &'a str, reason: Option<&'a str> },
}

/// The scope an action needs on a platform.
pub(crate) fn scope_for(platform: Platform, action: &Action<'_>) -> &'static str {
    match (platform, action) {
        (Platform::Kick, Action::Delete { .. }) => "moderation:chat_message:manage",
        (Platform::Kick, _) => "moderation:ban",
        (_, Action::Delete { .. }) => "moderator:manage:chat_messages",
        (_, _) => "moderator:manage:banned_users",
    }
}

fn label(platform: Platform) -> &'static str {
    match platform {
        Platform::Kick => "Kick",
        Platform::Twitch => "Twitch",
        Platform::Youtube => "YouTube",
        _ => "this platform",
    }
}

fn not_connected(platform: Platform) -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "cloud_not_connected",
        format!("The cloud bot isn't in your {} chat.", label(platform)),
    )
    .hint("Do this once your PC is on, or connect that platform for the cloud bot on the PC's Bot page.")
}

pub(crate) fn needs_scope(platform: Platform) -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "needs_moderation_scope",
        format!("The cloud bot can't moderate {} yet.", label(platform)),
    )
    .hint("On your PC, open Bot › Keep the bot on when this PC is off and turn on “Let the cloud bot moderate when this PC is off”.")
}

fn signed_out(platform: Platform) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "cloud_signed_out", format!("{} signed the cloud bot out.", label(platform)))
        .hint("Connect it again on your PC's Bot page.")
}

fn not_moderator(platform: Platform, bot: &str) -> ApiError {
    let hint = if platform == Platform::Twitch && !bot.is_empty() {
        format!("Make {bot} a moderator (type /mod {bot} in your chat), then try again.")
    } else {
        "Make the bot account a moderator in your channel, then try again.".to_string()
    };
    ApiError::new(
        StatusCode::FORBIDDEN,
        "bot_not_moderator",
        format!("{} says the cloud bot can't moderate in your channel.", label(platform)),
    )
    .hint(hint)
}

fn refused(platform: Platform, why: &str) -> ApiError {
    let why = why.trim();
    let message = if why.is_empty() {
        format!("{} didn't take that.", label(platform))
    } else {
        format!("{} didn't take that: {why}", label(platform))
    };
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "platform_refused", message).hint("Nothing was changed.")
}

fn gone(action: &Action<'_>) -> ApiError {
    match action {
        Action::Delete { .. } => {
            ApiError::new(StatusCode::NOT_FOUND, "message_not_found", "That message is gone already.")
                .hint("Platforms only delete messages from the last few hours.")
        }
        _ => ApiError::new(StatusCode::NOT_FOUND, "viewer_not_found", "The platform doesn't know that viewer.")
            .hint("They may have changed their name or left. Nothing was changed."),
    }
}

fn unreachable(platform: Platform) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, "platform_unreachable", format!("Couldn't reach {}.", label(platform)))
        .hint("Nothing was changed. Try again in a moment.")
}

fn platform_rate(platform: Platform, retry_after_s: u64) -> ApiError {
    ApiError::new(StatusCode::TOO_MANY_REQUESTS, "platform_rate", format!("{} asked to slow down.", label(platform)))
        .hint("Wait a few seconds and try again.")
        .header(header::RETRY_AFTER, retry_after_s.max(1).to_string())
}

fn busy(platform: Platform) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "platform_busy", format!("{} is busy with this viewer.", label(platform)))
        .hint("Try again in a moment.")
}

fn from_send(platform: Platform, e: SendErr) -> ApiError {
    match e {
        SendErr::SignedOut => signed_out(platform),
        SendErr::RateLimited(s) => platform_rate(platform, s),
        SendErr::NotReady => super::platform_unavailable(key(platform)),
        SendErr::Refused(m) | SendErr::Dropped(m) => refused(platform, &m),
        SendErr::Unavailable(_) => unreachable(platform),
    }
}

/// A ban, timeout or delete in `sub`'s `platform` channel, by the cloud bot.
pub(crate) async fn run(st: &AppState, sub: &str, platform: Platform, action: Action<'_>) -> Result<(), ApiError> {
    let p = key(platform);
    if !matches!(platform, Platform::Twitch | Platform::Kick) {
        return Err(not_connected(platform));
    }
    if !st.bot.has(platform) {
        return Err(super::platform_unavailable(p));
    }
    let ch = st.db.bot_channel(sub, p).map_err(ApiError::db)?.filter(store::Channel::ready);
    let Some(ch) = ch else { return Err(not_connected(platform)) };
    let needed = scope_for(platform, &action);
    if !ch.moderation || !ch.scopes.split_whitespace().any(|s| s == needed) {
        return Err(needs_scope(platform));
    }
    if let (Platform::Kick, Action::Timeout { seconds, .. }) = (platform, &action)
        && *seconds > KICK_MAX_TIMEOUT_S
    {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "invalid", "seconds: Kick timeouts can be up to 7 days."));
    }
    // The channel's 20 a minute: replies and moderation together.
    match st.db.try_take(sub, &format!("bot_messages_{p}"), Period::Minute, 1, MESSAGES_PER_MINUTE) {
        Ok(()) => {}
        Err(TakeErr::Over(q)) => {
            let wait = u64::try_from(q.reset_at - now_ms()).unwrap_or(0).div_ceil(1000).max(1);
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "cloud_rate",
                format!("The cloud bot did {MESSAGES_PER_MINUTE} things in {} chat this minute.", label(platform)),
            )
            .hint("Wait a moment and try again.")
            .header(HeaderName::from_static("x-quota-reset"), q.reset_at.to_string())
            .header(header::RETRY_AFTER, wait.to_string()));
        }
        Err(TakeErr::Db(e)) => return Err(ApiError::db(e)),
    }
    match platform {
        Platform::Twitch => twitch(st, &ch, action).await,
        _ => kick(st, &ch, action).await,
    }
}

/// Twitch said the bot account's sign-in is gone: the channel shows it (like the hourly
/// check) and the phone hears 409 `cloud_signed_out`.
fn twitch_signed_out(st: &AppState, sub: &str) -> ApiError {
    let err = super::error_json(
        "twitch_signed_out",
        "Twitch signed the bot account out.",
        "Disconnect Twitch here and connect it again.",
    );
    let _ = st.db.bot_set_error(sub, "twitch", Some(&err));
    signed_out(Platform::Twitch)
}

/// The bot account's own Twitch token: cached, else refreshed from the sealed sign-in (a
/// rotated refresh token is sealed again). One refresh at a time per subscription (the
/// cache is looked at again under the lock, so phones banning at once share one refresh and
/// a rotated refresh token is never used twice). Twitch refusing the refresh marks the
/// channel signed out, like the hourly check.
async fn twitch_user(st: &AppState, ch: &store::Channel) -> Result<String, ApiError> {
    if let Some(t) = st.bot.twitch_user(&ch.sub_id, now_ms()) {
        return Ok(t);
    }
    let api = st.bot.twitch()?;
    let lock = st.bot.twitch_lock(&ch.sub_id);
    let _one = lock.lock().await;
    if let Some(t) = st.bot.twitch_user(&ch.sub_id, now_ms()) {
        return Ok(t);
    }
    let refresh = super::open_token(st, &ch.sub_id, "twitch", "bot").ok_or_else(|| signed_out(Platform::Twitch))?;
    match api.refresh(&refresh).await {
        Ok(t) => {
            if !t.refresh_token.is_empty() && t.refresh_token != refresh {
                super::seal_token(st, &ch.sub_id, "twitch", "bot", &t.refresh_token);
            }
            st.bot.cache_twitch_user(&ch.sub_id, &t.access_token, now_ms() + TWITCH_USER_TTL_MS);
            Ok(t.access_token)
        }
        Err(TwitchError::Unauthorized) => Err(twitch_signed_out(st, &ch.sub_id)),
        Err(TwitchError::RateLimited { retry_after_s }) => Err(platform_rate(Platform::Twitch, retry_after_s)),
        Err(TwitchError::Unavailable(_)) => Err(unreachable(Platform::Twitch)),
        // A refresh token Twitch won't take in any other way is a sign-in to make again.
        Err(_) => Err(signed_out(Platform::Twitch)),
    }
}

/// The platform says the viewer is already banned or timed out: the phone asked for what's
/// already so, which is a success (a second phone, or a retry after a lost answer).
fn already(m: &str) -> bool {
    let m = m.to_ascii_lowercase();
    m.contains("already banned") || m.contains("already timed out") || m.contains("already been banned")
}

fn twitch_err(ch: &store::Channel, action: &Action<'_>, e: TwitchError) -> ApiError {
    let p = Platform::Twitch;
    match e {
        TwitchError::Unauthorized => signed_out(p),
        TwitchError::Forbidden(_) => not_moderator(p, ch.bot_login.as_deref().unwrap_or("")),
        TwitchError::BadRequest(m) | TwitchError::Dropped(m) => refused(p, &m),
        TwitchError::NotFound => gone(action),
        TwitchError::Conflict => busy(p),
        TwitchError::RateLimited { retry_after_s } => platform_rate(p, retry_after_s),
        TwitchError::Unavailable(_) => unreachable(p),
    }
}

async fn twitch(st: &AppState, ch: &store::Channel, action: Action<'_>) -> Result<(), ApiError> {
    let (Some(bot), Some(channel)) = (ch.bot_id.as_deref(), ch.channel_id.as_deref()) else {
        return Err(not_connected(Platform::Twitch));
    };
    let api = st.bot.twitch()?;
    let call = |token: String| async move {
        match action {
            Action::Delete { message_id } => {
                let target = ModTarget { broadcaster: channel, moderator: bot, user_id: "", reason: None };
                api.delete_message(&token, target, message_id).await
            }
            Action::Timeout { user_id, seconds, reason } => {
                api.ban(&token, ModTarget { broadcaster: channel, moderator: bot, user_id, reason }, Some(seconds))
                    .await
            }
            Action::Ban { user_id, reason } => {
                api.ban(&token, ModTarget { broadcaster: channel, moderator: bot, user_id, reason }, None).await
            }
        }
    };
    let token = twitch_user(st, ch).await?;
    let res = match call(token.clone()).await {
        Err(TwitchError::Unauthorized) => {
            // The cached token ended early: once more with a fresh one (unless another
            // request refreshed it meanwhile).
            st.bot.forget_twitch_user_if(&ch.sub_id, &token);
            let token = twitch_user(st, ch).await?;
            match call(token).await {
                // A fresh token refused too: the sign-in itself is gone.
                Err(TwitchError::Unauthorized) => return Err(twitch_signed_out(st, &ch.sub_id)),
                r => r,
            }
        }
        r => r,
    };
    match res {
        Ok(()) => Ok(()),
        Err(TwitchError::BadRequest(m) | TwitchError::Dropped(m))
            if !matches!(action, Action::Delete { .. }) && already(&m) =>
        {
            Ok(())
        }
        Err(e) => Err(twitch_err(ch, &action, e)),
    }
}

fn kick_err(action: &Action<'_>, e: KickError) -> ApiError {
    let p = Platform::Kick;
    match e {
        KickError::Unauthorized => signed_out(p),
        KickError::Forbidden(_) => not_moderator(p, ""),
        KickError::Dropped(m) => refused(p, &m),
        KickError::RateLimited { retry_after_s } => platform_rate(p, retry_after_s),
        KickError::Unavailable(m) if m.ends_with("404") => gone(action),
        KickError::Unavailable(_) => unreachable(p),
    }
}

async fn kick(st: &AppState, ch: &store::Channel, action: Action<'_>) -> Result<(), ApiError> {
    let Some(channel) = ch.channel_id.as_deref() else { return Err(not_connected(Platform::Kick)) };
    let api = st.bot.kick()?;
    let call = |token: String| async move {
        match action {
            Action::Delete { message_id } => api.delete_message(&token, message_id).await,
            Action::Timeout { user_id, seconds, reason } => {
                api.ban(&token, channel, user_id, Some(seconds.div_ceil(60).max(1)), reason).await
            }
            Action::Ban { user_id, reason } => api.ban(&token, channel, user_id, None, reason).await,
        }
    };
    let token = super::kick::access(st, &ch.sub_id).await.map_err(|e| from_send(Platform::Kick, e))?;
    let res = match call(token.clone()).await {
        Err(KickError::Unauthorized) => {
            st.bot.forget_kick_access_if(&ch.sub_id, &token);
            let token = super::kick::access(st, &ch.sub_id).await.map_err(|e| from_send(Platform::Kick, e))?;
            call(token).await
        }
        r => r,
    };
    match res {
        Ok(()) => Ok(()),
        Err(KickError::Dropped(m) | KickError::Forbidden(m))
            if !matches!(action, Action::Delete { .. }) && already(&m) =>
        {
            Ok(())
        }
        Err(e) => Err(kick_err(&action, e)),
    }
}
