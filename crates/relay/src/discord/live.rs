//! Cloud go-live posts: when the desktop (later the cloud bot) says a platform went live, the
//! Ferrocast bot posts an embed in the chosen channel (title, game, thumbnail, link) and edits
//! it when the stream ends (how long, peak viewers). One post per platform per 30 minutes: a
//! stream that drops and comes back gets its message back, not a new one. A report whose
//! `started_at` is over 30 minutes after the open post's is a new stream (the old end never
//! arrived): the old message is closed and a new one posted, so a lost end can't silence the
//! next day's post. Reports for one platform are handled one at a time.
//!
//! The answer says whether it posted, so the desktop can fall back to its own free webhook
//! post when the bot can't (`reason`). Never pings anyone, never carries Ferrocast branding
//! (R-06).

use serde::Deserialize;
use serde_json::{Value, json};

use super::api::{DiscordApi, DiscordError, codes};
use super::store::{GuildRow, PostRow};
use super::{clean, discord_error};
use crate::{ApiError, AppState, now_ms};
use axum::http::StatusCode;

/// A stream back within this long of its end is the same stream (the desktop's rule too).
pub const REPOST_AFTER_MS: i64 = 30 * 60_000;
/// A live post this old without an end is stale (the end was never reported).
const STALE_LIVE_MS: i64 = 24 * 3_600_000;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LiveIn {
    pub platform: String,
    pub live: bool,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub game: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub thumbnail_url: Option<String>,
    pub started_at: i64,
    #[serde(default)]
    pub peak_viewers: Option<u64>,
    /// With `live: false`: when it ended, if the report was queued (the desktop's clock).
    #[serde(default)]
    pub ended_at: Option<i64>,
}

fn invalid(m: &str) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "invalid", m)
}

/// Only links to the stream itself; anything else is dropped.
fn stream_url(raw: Option<&str>) -> Option<String> {
    let u = url::Url::parse(raw?.trim()).ok()?;
    let host = u.host_str()?.to_ascii_lowercase();
    let ok = ["twitch.tv", "www.twitch.tv", "youtube.com", "www.youtube.com", "youtu.be", "kick.com", "www.kick.com"];
    (u.scheme() == "https" && ok.contains(&host.as_str()) && u.as_str().len() <= 300).then(|| u.to_string())
}

/// The platform's own preview image, with a cache-buster so Discord shows today's frame.
fn thumbnail(raw: Option<&str>, at: i64) -> Option<String> {
    let mut u = url::Url::parse(raw?.trim()).ok()?;
    let host = u.host_str()?.to_ascii_lowercase();
    let ok = ["static-cdn.jtvnw.net", "i.ytimg.com", "images.kick.com", "files.kick.com"];
    if u.scheme() != "https" || !ok.contains(&host.as_str()) || u.as_str().len() > 300 {
        return None;
    }
    u.query_pairs_mut().append_pair("t", &at.to_string());
    Some(u.to_string())
}

fn platform_label(p: &str) -> &'static str {
    match p {
        "twitch" => "Twitch",
        "youtube" => "YouTube",
        _ => "Kick",
    }
}

fn platform_color(p: &str) -> i64 {
    match p {
        "twitch" => 0x9146FF,
        "youtube" => 0xFF0000,
        _ => 0x53FC18,
    }
}

/// `2026-10-19T12:05:00Z`.
pub(crate) fn iso(ms: i64) -> String {
    let (y, m, d) = crate::quota::civil(ms.div_euclid(86_400_000));
    let secs = ms.rem_euclid(86_400_000) / 1000;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, secs / 60 % 60, secs % 60)
}

/// "2 h 14 min", "45 min".
pub(crate) fn duration(ms: i64) -> String {
    let mins = (ms.max(0) + 30_000) / 60_000;
    match (mins / 60, mins % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

/// What a post shows, kept to rebuild the message at the end.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub(crate) struct Shown {
    platform: String,
    name: String,
    title: Option<String>,
    game: Option<String>,
    url: Option<String>,
    thumbnail: Option<String>,
    message: Option<String>,
}

fn live_body(s: &Shown, started_at: i64) -> Value {
    let label = platform_label(&s.platform);
    let mut embed = json!({
        "title": s.title.clone().unwrap_or_else(|| format!("{} is live on {label}", s.name)),
        "description": format!("{} is live on {label}.", s.name),
        "color": platform_color(&s.platform),
        "timestamp": iso(started_at),
    });
    if let Some(u) = &s.url {
        embed["url"] = json!(u);
    }
    if let Some(g) = &s.game {
        embed["fields"] = json!([{ "name": "Playing", "value": g, "inline": true }]);
    }
    if let Some(t) = &s.thumbnail {
        embed["image"] = json!({ "url": t });
    }
    let mut body = json!({ "embeds": [embed], "allowed_mentions": { "parse": [] } });
    if let Some(m) = &s.message {
        body["content"] = json!(m);
    }
    if let Some(u) = &s.url {
        body["components"] = json!([{ "type": 1, "components": [{ "type": 2, "style": 5, "label": format!("Watch on {label}"), "url": u }] }]);
    }
    body
}

/// The message once the stream is over; `ended_at` `None` when the end was never reported
/// (then no length is shown rather than a wrong one).
fn ended_body(s: &Shown, started_at: i64, ended_at: Option<i64>, peak: Option<u64>) -> Value {
    let label = platform_label(&s.platform);
    let mut fields = Vec::new();
    if let Some(end) = ended_at {
        fields.push(json!({ "name": "Streamed for", "value": duration(end - started_at), "inline": true }));
    }
    if let Some(p) = peak.filter(|p| *p > 0) {
        fields.push(json!({ "name": "Peak viewers", "value": p.to_string(), "inline": true }));
    }
    if let Some(g) = &s.game {
        fields.push(json!({ "name": "Played", "value": g, "inline": true }));
    }
    let mut embed = json!({
        "title": s.title.clone().unwrap_or_else(|| format!("{} was live on {label}", s.name)),
        "description": format!("{} was live on {label}. The stream has ended.", s.name),
        "color": 0x5C6370,
        "fields": fields,
        "timestamp": iso(started_at),
    });
    if let Some(u) = &s.url {
        embed["url"] = json!(u);
    }
    let mut body = json!({ "embeds": [embed], "allowed_mentions": { "parse": [] }, "components": [] });
    if let Some(m) = &s.message {
        body["content"] = json!(m);
    }
    body
}

/// `POST /v1/pro/discord/live`. One report at a time per subscription and platform.
pub(crate) async fn report(st: &AppState, sub: &str, b: LiveIn) -> Result<Value, ApiError> {
    if !super::perks::PLATFORMS.contains(&b.platform.as_str()) {
        return Err(invalid("platform: twitch, youtube or kick."));
    }
    let now = now_ms();
    if b.started_at <= 0 || b.started_at > now + 5 * 60_000 {
        return Err(invalid("started_at: when the stream started (ms), not in the future."));
    }
    let Some(g) = st.db.discord_guild(sub).map_err(ApiError::db)? else { return Ok(not("not_connected")) };
    let api = st.discord.api()?;
    let lock = st.discord.live_lock(sub, &b.platform);
    let _one = lock.lock().await;
    let last = st.db.discord_post(sub, &b.platform).map_err(ApiError::db)?;
    if b.live { went_live(st, api, sub, &g, &b, last, now).await } else { ended(st, api, sub, &b, last, now).await }
}

fn not(reason: &str) -> Value {
    json!({ "posted": false, "edited": false, "reason": reason })
}

/// The end: edit the open post, whatever the switch says now (it was on when it posted).
async fn ended(
    st: &AppState,
    api: &dyn DiscordApi,
    sub: &str,
    b: &LiveIn,
    last: Option<PostRow>,
    now: i64,
) -> Result<Value, ApiError> {
    let Some(mut p) = last.filter(|p| p.ended_at.is_none()) else { return Ok(not("no_post")) };
    // A late end for a stream before the open post's: leave the newer post alone.
    if b.started_at.saturating_add(REPOST_AFTER_MS) < p.started_at {
        return Ok(not("no_post"));
    }
    // A queued end comes with when it really ended; never before the start or after now.
    let end = b.ended_at.unwrap_or(now).clamp(p.started_at, now.max(p.started_at));
    p.ended_at = Some(end);
    let Ok(shown) = serde_json::from_str::<Shown>(&p.post) else {
        // Can't rebuild the message: just close it here, so the next stream posts afresh.
        tracing::warn!(platform = %b.platform, "a saved go-live post is unreadable; marked ended");
        st.db.discord_save_post(sub, &b.platform, &p).map_err(ApiError::db)?;
        return Ok(not("no_post"));
    };
    match api.edit(&p.channel_id, &p.message_id, ended_body(&shown, p.started_at, Some(end), b.peak_viewers)).await {
        Ok(()) => {}
        // Someone deleted the message or the channel: nothing to edit.
        Err(DiscordError::NotFound(_)) => {}
        Err(e) => return Err(discord_error(e)),
    }
    st.db.discord_save_post(sub, &b.platform, &p).map_err(ApiError::db)?;
    Ok(json!({ "posted": false, "edited": true, "reason": null }))
}

async fn went_live(
    st: &AppState,
    api: &dyn DiscordApi,
    sub: &str,
    g: &GuildRow,
    b: &LiveIn,
    last: Option<PostRow>,
    now: i64,
) -> Result<Value, ApiError> {
    if !g.go_live {
        return Ok(not("off"));
    }
    let Some(channel) = g.channel_id.clone() else { return Ok(not("no_channel")) };
    let name = clean(b.name.as_deref().unwrap_or(""), 80);
    let shown = Shown {
        platform: b.platform.clone(),
        name: if name.is_empty() { "The stream".into() } else { name },
        title: Some(clean(b.title.as_deref().unwrap_or(""), 256)).filter(|t| !t.is_empty()),
        game: Some(clean(b.game.as_deref().unwrap_or(""), 100)).filter(|t| !t.is_empty()),
        url: stream_url(b.url.as_deref()),
        thumbnail: thumbnail(b.thumbnail_url.as_deref(), now),
        message: g.go_live_message.clone(),
    };
    if let Some(mut p) = last {
        match p.ended_at {
            // Still live (the platform said "online" twice, or the desktop restarted): the same
            // stream when it started about when the post's did.
            None if now - p.posted_at < STALE_LIVE_MS && b.started_at <= p.started_at + REPOST_AFTER_MS => {
                return Ok(not("recent"));
            }
            // Open, but this stream started long after it: the end never arrived (the PC was
            // off, the report was lost). Close the old message as best we can; a new one follows.
            None => {
                if let Ok(saved) = serde_json::from_str::<Shown>(&p.post)
                    && let Err(e) =
                        api.edit(&p.channel_id, &p.message_id, ended_body(&saved, p.started_at, None, None)).await
                {
                    tracing::debug!(error = %e, "couldn't close an old go-live post");
                }
            }
            // Back within 30 minutes: the same message shows live again.
            Some(end)
                if now - end < REPOST_AFTER_MS && b.started_at < end + REPOST_AFTER_MS && p.channel_id == channel =>
            {
                let saved: Shown = serde_json::from_str(&p.post).unwrap_or_else(|_| shown.clone());
                match api.edit(&p.channel_id, &p.message_id, live_body(&saved, p.started_at)).await {
                    Ok(()) => {
                        p.ended_at = None;
                        p.posted_at = now;
                        if let Err(e) = st.db.discord_save_post(sub, &b.platform, &p) {
                            // Discord shows it live again: say so, or the webhook would post too.
                            tracing::warn!(error = %e, "couldn't save a resumed go-live post");
                        }
                        return Ok(json!({ "posted": false, "edited": true, "reason": "resumed" }));
                    }
                    Err(DiscordError::NotFound(_)) => {} // deleted: post a new one
                    Err(e) => return Err(discord_error(e)),
                }
            }
            _ => {}
        }
    }
    let message_id = match api.post(&channel, live_body(&shown, b.started_at)).await {
        Ok(id) => id,
        Err(DiscordError::NotFound(codes::UNKNOWN_CHANNEL)) => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "channel_not_found",
                "The go-live channel doesn't exist any more.",
            )
            .hint("Pick another channel in Settings › Discord."));
        }
        Err(e) => return Err(discord_error(e)),
    };
    let post = serde_json::to_string(&shown).unwrap_or_default();
    let row =
        PostRow { channel_id: channel, message_id, started_at: b.started_at, posted_at: now, ended_at: None, post };
    // Discord already shows the post: a failed save must not make the desktop's webhook post
    // a second one. (The end then finds no post to edit, which only leaves it saying "live".)
    if let Err(e) = st.db.discord_save_post(sub, &b.platform, &row) {
        tracing::warn!(error = %e, "couldn't save a go-live post Discord accepted");
    }
    Ok(json!({ "posted": true, "edited": false, "reason": null }))
}
