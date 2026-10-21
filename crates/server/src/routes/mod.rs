//! `/api/v1` routes (API.md §4).

mod app;
mod autoreplies;
mod bot;
mod cloud_backups;
mod cloud_bot;
mod cloud_sync;
mod core;
mod designs;
mod discord;
mod giveaways;
mod history;
mod import;
mod loyalty;
#[cfg(test)]
pub(crate) use core::lan_ip;
mod media;
mod moderation;
mod polls;
mod pro;
mod recap;
pub(crate) mod remote_scope;
mod smart;
mod studio;
mod tip_pro;
mod tts;
mod weekly_email;
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
        .route("/tts/voices", get(tts::voices))
        .route("/tts/cloud", get(tts::cloud).put(tts::set_cloud))
        .route("/tts/preview", post(tts::preview))
        .route("/tts/{id}/approve", post(bot::tts_approve))
        .route("/tts/{id}/reject", post(bot::tts_reject))
        // media list/delete (upload is in `uploads` with its own size limit)
        .route("/media", get(media::list))
        .route("/media/{id}", get(crate::assets::dashboard_media).delete(media::remove))
        .route("/live", get(crate::ws::live))
        // tips
        .route("/tips", get(core::tips).put(core::save_tips).delete(core::delete_tips))
        // Pro: tip page branding and goal (the images are in `uploads`)
        .route("/tips/brand", get(tip_pro::brand).put(tip_pro::save_brand).delete(tip_pro::reset_brand))
        .route("/tips/goal", get(tip_pro::goal).put(tip_pro::save_goal).delete(tip_pro::delete_goal))
        // Pro: the tip page's own address (Cloudflare for SaaS on the relay)
        .route("/pro/tips/domain", get(tip_pro::domain).put(tip_pro::save_domain).delete(tip_pro::delete_domain))
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
        .route("/recap/summary", get(smart::recap_saved).post(smart::recap))
        .route("/insights/supporters", get(smart::supporters))
        .route("/updates", get(app::updates))
        .route("/updates/check", post(app::check_updates))
        .route("/updates/install", post(app::install_update))
        .route("/updates/restart", post(app::restart_for_update))
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
        .route("/pro/redeem", post(pro::redeem))
        // The Ferrocast account (optional; ACCOUNTS.md). Under /pro so a phone never reaches it.
        .route("/pro/account", get(pro::account))
        .route("/pro/account/sign-in", post(pro::account_sign_in))
        .route("/pro/account/sign-in/cancel", post(pro::account_sign_in_cancel))
        .route("/pro/account/sign-out", post(pro::account_sign_out))
        .route("/pro/account/sessions", get(pro::account_sessions))
        .route("/pro/account/sessions/{id}", delete(pro::account_revoke))
        // Cloud sync and "Use cloud overlays" (CLOUD.md §1.1): free, with the account.
        .route("/pro/cloud", get(cloud_sync::status).put(cloud_sync::set))
        .route("/pro/cloud/sync", post(cloud_sync::sync))
        .route("/pro/history", get(history::list))
        .route("/pro/history/export.csv", get(history::export))
        .route("/pro/history/samples", get(history::samples))
        // Pro: the weekly email (see routes/weekly_email.rs)
        .route("/pro/history/email", get(weekly_email::view).put(weekly_email::save).delete(weekly_email::stop))
        // Pro: smart moderation, smart replies (see routes/smart.rs)
        .route("/pro/smart", get(smart::view).put(smart::save))
        .route("/pro/smart/about/approve", post(smart::approve))
        .route("/pro/smart/moderation", get(smart::log))
        .route("/pro/smart/moderation/{id}/apply", post(smart::apply))
        .route("/pro/smart/moderation/{id}/undo", post(smart::undo))
        .route("/pro/smart/moderation/{id}/dismiss", post(smart::dismiss))
        // Pro: the 24/7 cloud bot (see routes/cloud_bot.rs)
        .route("/pro/bot", get(cloud_bot::view))
        .route("/pro/bot/enabled", put(cloud_bot::enabled))
        .route("/pro/bot/{platform}/connect", post(cloud_bot::connect))
        .route("/pro/bot/{platform}", delete(cloud_bot::disconnect))
        // Pro: the Ferrocast bot for Discord (see routes/discord.rs)
        .route("/pro/discord", get(discord::bot).delete(discord::bot_disconnect))
        .route("/pro/discord/connect", post(discord::bot_connect))
        .route("/pro/discord/channel", put(discord::bot_channel))
        .route("/pro/discord/go-live", put(discord::bot_go_live))
        .route("/pro/discord/rules", put(discord::bot_add_rule))
        .route("/pro/discord/rules/{id}", put(discord::bot_update_rule).delete(discord::bot_delete_rule))
        // Pro cloud backups (see routes/cloud_backups.rs)
        .route("/pro/backups", get(cloud_backups::list).post(cloud_backups::now))
        .route("/pro/backups/key", get(cloud_backups::key))
        .route("/pro/backups/key/saved", post(cloud_backups::key_saved))
        .route("/pro/backups/passphrase", put(cloud_backups::set_passphrase).delete(cloud_backups::clear_passphrase))
        .route("/pro/backups/settings", put(cloud_backups::settings))
        .route("/pro/backups/{id}", delete(cloud_backups::remove))
        .route("/pro/backups/{id}/restore", post(cloud_backups::restore))
        // Studio: counters, quotes, modules, stream info, show, OBS helpers, moments
        .merge(studio::routes())
        // auto-replies (bot answers to chat that mentions something)
        .merge(autoreplies::routes())
        // Pro dashboard from anywhere: devices, pairing links (see crate::remote)
        .merge(crate::remote::routes())
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
        .merge(
            Router::new()
                .route("/tips/brand/{kind}", put(tip_pro::save_image).delete(tip_pro::delete_image))
                .layer(axum::extract::DefaultBodyLimit::max(tip_pro::MAX_IMAGE_BODY)),
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
