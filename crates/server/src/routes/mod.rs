//! `/api/v1` routes (API.md §4).

mod app;
mod bot;
mod core;
mod designs;
mod discord;
mod giveaways;
mod import;
mod loyalty;
#[cfg(test)]
pub(crate) use core::lan_ip;
mod media;
mod moderation;
mod polls;
mod pro;
mod recap;
mod studio;
mod welcome;

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
        // designs (gallery, designer, Use, share files)
        .route("/designs", get(designs::list).post(designs::create))
        .route("/designs/active-alert", get(designs::active_alert).put(designs::set_active_alert))
        .route("/designs/{id}", get(designs::get).put(designs::update).delete(designs::remove))
        .route("/designs/{id}/duplicate", post(designs::duplicate))
        .route("/designs/{id}/use", post(designs::use_design))
        .route("/designs/{id}/export", post(designs::export))
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
        .route("/mod/panic/config", get(moderation::panic_config).put(moderation::save_panic))
        .route("/mod/held", get(moderation::held))
        .route("/mod/held/{event_id}/release", post(moderation::release_held))
        .route("/mod/held/{event_id}/drop", post(moderation::drop_held))
        .route("/mod/filters", get(moderation::filters).put(moderation::save_filters))
        .route("/viewers/{platform}/{user_id}", get(moderation::viewer))
        .route("/viewers/{platform}/{user_id}/note", get(moderation::note).put(moderation::save_note))
        // bot
        .route("/commands", get(bot::commands).post(bot::create_command))
        .route("/commands/starter", post(bot::add_starter))
        .route("/commands/{id}", put(bot::update_command).delete(bot::delete_command))
        .route("/timers", get(bot::timers).post(bot::create_timer))
        .route("/timers/{id}", put(bot::update_timer).delete(bot::delete_timer))
        // loyalty points
        .route("/loyalty", get(loyalty::config).put(loyalty::save_config))
        .route("/loyalty/leaderboard", get(loyalty::leaderboard))
        .route("/loyalty/viewers/{platform}/{user_id}", get(loyalty::balance))
        .route("/loyalty/viewers/{platform}/{user_id}/adjust", post(loyalty::adjust))
        // polls
        .route("/polls", post(polls::start))
        .route("/polls/current", get(polls::current).delete(polls::clear))
        .route("/polls/current/end", post(polls::end))
        // giveaways
        .route("/giveaways", post(giveaways::start))
        .route("/giveaways/current", get(giveaways::current).delete(giveaways::cancel))
        .route("/giveaways/current/close", post(giveaways::close))
        .route("/giveaways/current/draw", post(giveaways::draw))
        .route("/giveaways/current/redraw", post(giveaways::redraw))
        .route("/giveaways/current/end", post(giveaways::end))
        // first-time chatters
        .route("/welcome", get(welcome::config).put(welcome::save))
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
        .route("/tips", get(core::tips).put(core::save_tips).delete(core::delete_tips))
        // Discord go-live posts
        .route("/discord", get(discord::get).put(discord::save))
        .route("/discord/test", post(discord::test))
        // OBS
        .route("/obs", get(core::obs).put(core::save_obs))
        .route("/obs/detect", get(core::obs_detect).post(core::obs_use_detected))
        // settings and data
        .route("/settings", get(core::settings).put(core::save_settings))
        .route("/themes", get(app::themes))
        .route("/stats/session", get(app::session_stats))
        .route("/recap", get(recap::recap))
        .route("/updates", get(app::updates))
        .route("/updates/install", post(app::install_update))
        .route("/setup/complete", post(app::setup_complete))
        // StreamElements and Streamlabs importers (the Streamlabs upload is in `uploads`)
        .route("/import/streamelements/preview", post(import::streamelements_preview))
        .route("/import/apply", post(import::apply))
        .route("/backups", get(core::backups).post(core::backup_now))
        .route("/backups/{id}/restore", post(core::restore_backup))
        // Ferrocast Pro (through the relay; see routes/pro.rs)
        .route("/pro", get(pro::status))
        .route("/pro/refresh", post(pro::refresh))
        .route("/pro/checkout", post(pro::checkout))
        .route("/pro/manage", post(pro::manage))
        .route("/pro/cancel", post(pro::cancel))
        .route("/pro/restore", post(pro::restore))
        // Studio: counters, quotes, modules, stream info, show, OBS helpers, moments
        .merge(studio::routes())
}

/// Upload routes get a body limit just above the largest allowed file.
pub(crate) fn uploads() -> Router<AppState> {
    Router::new()
        .route("/media", post(media::upload))
        .route("/export", post(media::export))
        .layer(axum::extract::DefaultBodyLimit::max(media::MAX_UPLOAD + 64 * 1024))
        .merge(
            Router::new()
                .route("/import/streamlabs/preview", post(import::streamlabs_preview))
                .layer(axum::extract::DefaultBodyLimit::max(import::MAX_STREAMLABS_BODY)),
        )
        .merge(
            Router::new()
                .route("/designs/import", post(designs::import))
                .layer(axum::extract::DefaultBodyLimit::max(designs::MAX_DESIGN_BODY)),
        )
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

#[cfg(test)]
pub(crate) fn obs_view_for_tests(o: &ferrocast_engine::ObsView) -> serde_json::Value {
    core::obs_view(o)
}
