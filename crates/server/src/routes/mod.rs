//! `/api/v1` routes (API.md §4).

mod bot;
mod core;
mod media;
mod moderation;

use axum::Router;
use axum::routing::{delete, get, post, put};
use ferrocast_core::event::Platform;

use crate::AppState;
use crate::error::ApiError;

pub(crate) fn api() -> Router<AppState> {
    Router::new()
        // status and pairing
        .route("/status", get(core::status))
        .route("/session/pair", post(core::pair))
        .route("/session/pair-code", post(core::pair_code))
        .route("/accounts", get(core::accounts))
        .route("/accounts/{platform}/connect", post(core::connect))
        .route("/accounts/{platform}", delete(core::disconnect))
        // overlays
        .route("/overlays", get(core::overlays).post(core::create_overlay))
        .route("/overlays/{id}", get(core::overlay).put(core::update_overlay).delete(core::delete_overlay))
        .route("/overlays/{id}/regenerate-url", post(core::regenerate_url))
        .route("/overlays/{id}/add-to-obs", post(core::add_to_obs))
        // alerts and queue
        .route("/alert-rules", get(core::rules).post(core::create_rule))
        .route("/alert-rules/{id}", put(core::update_rule).delete(core::delete_rule))
        .route("/test", post(core::test))
        .route("/queue", get(core::queue))
        .route("/queue/pause", post(core::queue_pause))
        .route("/queue/resume", post(core::queue_resume))
        .route("/queue/skip", post(core::queue_skip))
        .route("/queue/clear", post(core::queue_clear))
        .route("/queue/{alert_id}/remove", post(core::queue_remove))
        .route("/events", get(core::events))
        .route("/events/{id}/replay", post(core::replay))
        .route("/events/{id}/why", get(core::why))
        // goals
        .route("/goals", get(core::goals).post(core::create_goal))
        .route("/goals/{id}", put(core::update_goal).delete(core::delete_goal))
        .route("/goals/{id}/adjust", post(core::adjust_goal))
        // chat and moderation
        .route("/chat", get(moderation::chat))
        .route("/chat/send", post(moderation::send))
        .route("/mod/delete", post(moderation::delete_message))
        .route("/mod/timeout", post(moderation::timeout))
        .route("/mod/ban", post(moderation::ban))
        .route("/mod/unban", post(moderation::unban))
        .route("/mod/bans", get(moderation::bans))
        .route("/mod/panic", get(moderation::panic_state).post(moderation::panic))
        .route("/mod/panic/config", put(moderation::save_panic))
        .route("/mod/filters", get(moderation::filters).put(moderation::save_filters))
        .route("/viewers/{platform}/{user_id}", get(moderation::viewer))
        .route("/viewers/{platform}/{user_id}/note", put(moderation::save_note))
        // bot
        .route("/commands", get(bot::commands).post(bot::create_command))
        .route("/commands/{id}", put(bot::update_command).delete(bot::delete_command))
        .route("/timers", get(bot::timers).post(bot::create_timer))
        .route("/timers/{id}", put(bot::update_timer).delete(bot::delete_timer))
        // tts
        .route("/tts", get(bot::tts).put(bot::save_tts))
        .route("/tts/pending", get(bot::tts_pending))
        .route("/tts/voices", get(bot::tts_voices))
        .route("/tts/{id}/approve", post(bot::tts_approve))
        .route("/tts/{id}/reject", post(bot::tts_reject))
        // media list/delete (upload is in `uploads` with its own size limit)
        .route("/media", get(media::list))
        .route("/media/{id}", get(crate::assets::dashboard_media).delete(media::remove))
        .route("/live", get(crate::ws::live))
        // tips
        .route("/tips", get(core::tips).put(core::save_tips))
        // OBS
        .route("/obs", get(core::obs).put(core::save_obs))
        // settings and data
        .route("/settings", get(core::settings).put(core::save_settings))
        .route("/backups", get(core::backups).post(core::backup_now))
        .route("/backups/{id}/restore", post(core::restore_backup))
}

/// Upload routes get a body limit just above the largest allowed file.
pub(crate) fn uploads() -> Router<AppState> {
    Router::new()
        .route("/media", post(media::upload))
        .route("/export", post(media::export))
        .layer(axum::extract::DefaultBodyLimit::max(media::MAX_UPLOAD + 64 * 1024))
        .merge(Router::new().route("/import", post(media::import)).layer(axum::extract::DefaultBodyLimit::max(
            usize::try_from(ferrocast_store::pack::MAX_PACK).unwrap_or(usize::MAX).saturating_add(64 * 1024),
        )))
}

/// Parses a platform from a path or body string.
pub(crate) fn platform(s: &str) -> Result<Platform, ApiError> {
    match s {
        "twitch" => Ok(Platform::Twitch),
        "youtube" => Ok(Platform::Youtube),
        "kick" => Ok(Platform::Kick),
        _ => Err(ApiError::invalid(format!("\"{s}\" isn't a platform; use twitch, youtube or kick."))),
    }
}
