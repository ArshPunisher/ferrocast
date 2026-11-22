//! Machine-readable OpenAPI 3.1.0 (REST) and AsyncAPI 3.0.0 (WebSocket) specifications
//! checked directly against registered Axum routes and engine live frames.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use axum::Json;
use ferrocast_core::api_keys::ApiKeyScope;
use serde_json::{Map, Value, json};

use crate::routes::remote_scope;

/// One HTTP REST operation exposed under `/api/v1`.
#[derive(Debug, Clone, Copy)]
pub struct RouteDoc {
    pub method: &'static str,
    pub rel_path: &'static str,
    pub tag: &'static str,
    pub summary: &'static str,
    pub req_schema: Option<&'static str>,
    pub resp_schema: Option<&'static str>,
}

const fn doc(
    method: &'static str,
    rel_path: &'static str,
    tag: &'static str,
    summary: &'static str,
    req_schema: Option<&'static str>,
    resp_schema: Option<&'static str>,
) -> RouteDoc {
    RouteDoc { method, rel_path, tag, summary, req_schema, resp_schema }
}

const CORE_AND_KEYS: &[RouteDoc] = &[
    doc(
        "GET",
        "/status",
        "Core",
        "Connector states, server port, version, and onboarding status.",
        None,
        Some("StatusResponse"),
    ),
    doc("GET", "/openapi.json", "Core", "OpenAPI 3.1.0 specification for the entire local REST API.", None, None),
    doc("GET", "/asyncapi.json", "Core", "AsyncAPI 3.0.0 specification for WebSocket streams.", None, None),
    doc(
        "POST",
        "/session/pair",
        "Core",
        "Exchange a 6-digit pairing code for a browser session bearer token.",
        Some("PairRequest"),
        Some("PairResponse"),
    ),
    doc(
        "POST",
        "/session/pair-code",
        "Core",
        "Generate a short-lived pairing code for another device or browser.",
        None,
        None,
    ),
    doc(
        "GET",
        "/api-keys",
        "API Keys",
        "List scoped local API keys (metadata only) and recent redacted usage logs.",
        None,
        Some("ApiKeysView"),
    ),
    doc(
        "POST",
        "/api-keys",
        "API Keys",
        "Create a scoped local API key. Returns the plain token once.",
        Some("ApiKeyCreate"),
        Some("ApiKeyCreated"),
    ),
    doc("DELETE", "/api-keys/{id}", "API Keys", "Revoke a local API key immediately.", None, None),
    doc("GET", "/themes", "Core", "List built-in overlay visual themes.", None, None),
    doc(
        "GET",
        "/stats/session",
        "Core",
        "Live session counters (follows, subs, tips, bits, chat volume, viewers).",
        None,
        None,
    ),
    doc("GET", "/recap", "Core", "Post-stream recap summary.", None, None),
    doc("GET", "/recap/summary", "Core", "Saved AI stream recap summary.", None, None),
    doc("POST", "/recap/summary", "Core", "Generate an AI summary for a completed stream.", None, None),
    doc("GET", "/credits", "Core", "End-credits roll entries for the current/latest stream.", None, None),
    doc("GET", "/insights/supporters", "Core", "Supporter trends across streams.", None, None),
];

const CHAT_AND_MOD: &[RouteDoc] = &[
    doc("GET", "/chat", "Chat & Moderation", "Recent merged chat messages.", None, None),
    doc(
        "POST",
        "/chat/send",
        "Chat & Moderation",
        "Send a chat message to connected platforms.",
        Some("ChatSendRequest"),
        None,
    ),
    doc("GET", "/chat/highlight", "Chat & Moderation", "Current pinned chat highlight.", None, None),
    doc("POST", "/chat/highlight", "Chat & Moderation", "Pin a chat message to overlays.", None, None),
    doc("DELETE", "/chat/highlight", "Chat & Moderation", "Clear the active chat highlight.", None, None),
    doc("POST", "/mod/delete", "Chat & Moderation", "Delete one chat message by id.", None, None),
    doc("POST", "/mod/timeout", "Chat & Moderation", "Timeout a chatter.", Some("ModTimeoutRequest"), None),
    doc("POST", "/mod/ban", "Chat & Moderation", "Ban a chatter.", Some("ModBanRequest"), None),
    doc("POST", "/mod/unban", "Chat & Moderation", "Unban a chatter.", Some("ModBanRequest"), None),
    doc("GET", "/mod/bans", "Chat & Moderation", "Active bans and timeouts.", None, None),
    doc("GET", "/mod/panic", "Chat & Moderation", "Current panic lockdown state.", None, None),
    doc("POST", "/mod/panic", "Chat & Moderation", "Turn panic lockdown mode on or off.", None, None),
    doc("GET", "/mod/panic/config", "Chat & Moderation", "Panic lockdown actions configuration.", None, None),
    doc("PUT", "/mod/panic/config", "Chat & Moderation", "Update panic lockdown configuration.", None, None),
    doc("GET", "/mod/held", "Chat & Moderation", "Messages/events currently held for review.", None, None),
    doc(
        "POST",
        "/mod/held/{event_id}/release",
        "Chat & Moderation",
        "Release a held chat message or event.",
        None,
        None,
    ),
    doc("POST", "/mod/held/{event_id}/drop", "Chat & Moderation", "Drop a held chat message or event.", None, None),
    doc("GET", "/mod/filters", "Chat & Moderation", "Word, link, and caps moderation filters.", None, None),
    doc("PUT", "/mod/filters", "Chat & Moderation", "Update moderation filters.", None, None),
    doc("GET", "/viewers/{platform}/{user_id}", "Chat & Moderation", "Viewer card, activity, and stats.", None, None),
    doc("GET", "/viewers/{platform}/{user_id}/note", "Chat & Moderation", "Moderator note on a viewer.", None, None),
    doc(
        "PUT",
        "/viewers/{platform}/{user_id}/note",
        "Chat & Moderation",
        "Save a moderator note on a viewer.",
        None,
        None,
    ),
];

const BOT_AND_WIDGETS: &[RouteDoc] = &[
    doc("GET", "/commands", "Bot & Widgets", "List custom chat commands.", None, None),
    doc("POST", "/commands", "Bot & Widgets", "Create a chat command.", None, None),
    doc("POST", "/commands/starter", "Bot & Widgets", "Install starter chat commands.", None, None),
    doc("PUT", "/commands/{id}", "Bot & Widgets", "Update a chat command.", None, None),
    doc("DELETE", "/commands/{id}", "Bot & Widgets", "Delete a chat command.", None, None),
    doc("GET", "/timers", "Bot & Widgets", "List recurring chat timers.", None, None),
    doc("POST", "/timers", "Bot & Widgets", "Create a recurring chat timer.", None, None),
    doc("PUT", "/timers/{id}", "Bot & Widgets", "Update a chat timer.", None, None),
    doc("DELETE", "/timers/{id}", "Bot & Widgets", "Delete a chat timer.", None, None),
    doc("GET", "/autoreplies", "Bot & Widgets", "List keyword/regex auto-replies.", None, None),
    doc("POST", "/autoreplies", "Bot & Widgets", "Create an auto-reply rule.", None, None),
    doc("PUT", "/autoreplies/order", "Bot & Widgets", "Reorder auto-reply priority.", None, None),
    doc("POST", "/autoreplies/try", "Bot & Widgets", "Dry-run a sample message against auto-replies.", None, None),
    doc("GET", "/autoreplies/starters", "Bot & Widgets", "Built-in starter auto-reply presets.", None, None),
    doc("PUT", "/autoreplies/{id}", "Bot & Widgets", "Update an auto-reply rule.", None, None),
    doc("DELETE", "/autoreplies/{id}", "Bot & Widgets", "Delete an auto-reply rule.", None, None),
    doc("GET", "/loyalty", "Bot & Widgets", "Loyalty points configuration.", None, None),
    doc("PUT", "/loyalty", "Bot & Widgets", "Update loyalty points configuration.", None, None),
    doc("GET", "/loyalty/leaderboard", "Bot & Widgets", "Top viewers by loyalty points.", None, None),
    doc("GET", "/loyalty/viewers/{platform}/{user_id}", "Bot & Widgets", "Viewer loyalty point balance.", None, None),
    doc(
        "POST",
        "/loyalty/viewers/{platform}/{user_id}/adjust",
        "Bot & Widgets",
        "Adjust a viewer's loyalty balance.",
        None,
        None,
    ),
    doc("POST", "/polls", "Bot & Widgets", "Start a live chat poll.", None, None),
    doc("GET", "/polls/current", "Bot & Widgets", "Current active or finished poll.", None, None),
    doc("DELETE", "/polls/current", "Bot & Widgets", "Clear the current poll widget.", None, None),
    doc("POST", "/polls/current/end", "Bot & Widgets", "End the active poll early.", None, None),
    doc("POST", "/giveaways", "Bot & Widgets", "Start a chat giveaway.", None, None),
    doc("GET", "/giveaways/current", "Bot & Widgets", "Current giveaway state and entries.", None, None),
    doc("DELETE", "/giveaways/current", "Bot & Widgets", "Cancel and refund the active giveaway.", None, None),
    doc("POST", "/giveaways/current/close", "Bot & Widgets", "Close giveaway entries.", None, None),
    doc("POST", "/giveaways/current/draw", "Bot & Widgets", "Draw a giveaway winner.", None, None),
    doc("POST", "/giveaways/current/redraw", "Bot & Widgets", "Redraw a giveaway winner.", None, None),
    doc("POST", "/giveaways/current/end", "Bot & Widgets", "Settle and close the giveaway.", None, None),
    doc("GET", "/welcome", "Bot & Widgets", "First-time chatter greeting configuration.", None, None),
    doc("PUT", "/welcome", "Bot & Widgets", "Update first-time chatter greeting.", None, None),
    doc("GET", "/tts", "Bot & Widgets", "Text-to-speech configuration.", None, None),
    doc("PUT", "/tts", "Bot & Widgets", "Update text-to-speech configuration.", None, None),
    doc("GET", "/tts/pending", "Bot & Widgets", "Pending TTS redemptions waiting for approval.", None, None),
    doc("GET", "/tts/voices", "Bot & Widgets", "Available local and cloud TTS voices.", None, None),
    doc("GET", "/tts/cloud", "Bot & Widgets", "Cloud TTS provider configuration.", None, None),
    doc("PUT", "/tts/cloud", "Bot & Widgets", "Update cloud TTS configuration.", None, None),
    doc("POST", "/tts/preview", "Bot & Widgets", "Speak a short TTS sample locally.", None, None),
    doc("POST", "/tts/{id}/approve", "Bot & Widgets", "Approve and speak a pending TTS item.", None, None),
    doc("POST", "/tts/{id}/reject", "Bot & Widgets", "Reject a pending TTS item.", None, None),
    doc("GET", "/counters", "Bot & Widgets", "List bot counters.", None, None),
    doc("POST", "/counters", "Bot & Widgets", "Create a counter.", None, None),
    doc("PUT", "/counters/{name}", "Bot & Widgets", "Set a counter value explicitly.", None, None),
    doc("DELETE", "/counters/{name}", "Bot & Widgets", "Delete a counter.", None, None),
    doc("POST", "/counters/{name}/change", "Bot & Widgets", "Increment or decrement a counter by delta.", None, None),
    doc("GET", "/quotes", "Bot & Widgets", "List stream quotes.", None, None),
    doc("POST", "/quotes", "Bot & Widgets", "Add a quote.", None, None),
    doc("PUT", "/quotes/{id}", "Bot & Widgets", "Edit a quote.", None, None),
    doc("DELETE", "/quotes/{id}", "Bot & Widgets", "Delete a quote.", None, None),
    doc("GET", "/modules", "Bot & Widgets", "Enabled/disabled bot module toggles.", None, None),
    doc("PUT", "/modules", "Bot & Widgets", "Save bot module toggles.", None, None),
];

const ALERTS_DESIGNS_MEDIA: &[RouteDoc] = &[
    doc("GET", "/overlays", "Alerts & Overlays", "List browser-source overlay endpoints.", None, None),
    doc("POST", "/overlays", "Alerts & Overlays", "Create a new overlay browser source.", None, None),
    doc("GET", "/overlays/{id}", "Alerts & Overlays", "Get one overlay's settings and URL.", None, None),
    doc("PUT", "/overlays/{id}", "Alerts & Overlays", "Update overlay layout or widgets.", None, None),
    doc("DELETE", "/overlays/{id}", "Alerts & Overlays", "Delete an overlay.", None, None),
    doc(
        "POST",
        "/overlays/{id}/regenerate-url",
        "Alerts & Overlays",
        "Rotate an overlay's browser-source token URL.",
        None,
        None,
    ),
    doc(
        "POST",
        "/overlays/{id}/add-to-obs",
        "Alerts & Overlays",
        "Add an overlay as a browser source in OBS.",
        None,
        None,
    ),
    doc(
        "POST",
        "/overlays/{id}/follow-design",
        "Alerts & Overlays",
        "Bind an overlay to follow the active design.",
        None,
        None,
    ),
    doc("GET", "/designs", "Alerts & Overlays", "List built-in and custom alert/scene designs.", None, None),
    doc("POST", "/designs", "Alerts & Overlays", "Create a new design.", None, None),
    doc("GET", "/designs/active-alert", "Alerts & Overlays", "Active alert design configuration.", None, None),
    doc("PUT", "/designs/active-alert", "Alerts & Overlays", "Set the active alert design.", None, None),
    doc("GET", "/designs/{id}", "Alerts & Overlays", "Fetch a single design definition.", None, None),
    doc("PUT", "/designs/{id}", "Alerts & Overlays", "Save edits to a design.", None, None),
    doc("DELETE", "/designs/{id}", "Alerts & Overlays", "Delete a custom design.", None, None),
    doc("POST", "/designs/{id}/duplicate", "Alerts & Overlays", "Duplicate an existing design.", None, None),
    doc("POST", "/designs/{id}/use", "Alerts & Overlays", "Apply a design across matching overlays.", None, None),
    doc(
        "GET",
        "/designs/{id}/obs",
        "Alerts & Overlays",
        "Preview what applying a design to OBS would change.",
        None,
        None,
    ),
    doc("POST", "/designs/{id}/obs", "Alerts & Overlays", "Install a design's scenes/sources into OBS.", None, None),
    doc("POST", "/designs/{id}/export", "Imports & Exports", "Export a design package file.", None, None),
    doc("POST", "/designs/import", "Imports & Exports", "Import a design package upload (PC only).", None, None),
    doc("GET", "/alert-rules", "Alerts & Overlays", "List configured alert rules.", None, None),
    doc("POST", "/alert-rules", "Alerts & Overlays", "Create an alert rule.", None, None),
    doc("PUT", "/alert-rules/{id}", "Alerts & Overlays", "Update an alert rule.", None, None),
    doc("DELETE", "/alert-rules/{id}", "Alerts & Overlays", "Delete an alert rule.", None, None),
    doc(
        "POST",
        "/test",
        "Alerts & Overlays",
        "Trigger a test alert through the live queue and overlays.",
        Some("TestAlertRequest"),
        None,
    ),
    doc("GET", "/queue", "Alerts & Overlays", "Current alert queue state.", None, None),
    doc("POST", "/queue/pause", "Alerts & Overlays", "Pause playback of upcoming queued alerts.", None, None),
    doc("POST", "/queue/resume", "Alerts & Overlays", "Resume paused alert queue playback.", None, None),
    doc("POST", "/queue/skip", "Alerts & Overlays", "Skip the currently displaying alert.", None, None),
    doc("POST", "/queue/mute", "Alerts & Overlays", "Mute alert audio without hiding visuals.", None, None),
    doc("POST", "/queue/unmute", "Alerts & Overlays", "Unmute alert audio.", None, None),
    doc("POST", "/queue/clear", "Alerts & Overlays", "Clear all pending queued alerts.", None, None),
    doc(
        "POST",
        "/queue/{alert_id}/remove",
        "Alerts & Overlays",
        "Remove a specific pending alert from the queue.",
        None,
        None,
    ),
    doc(
        "GET",
        "/events",
        "Alerts & Overlays",
        "Recent stream events ledger (follows, subs, tips, bits, raids).",
        None,
        None,
    ),
    doc("POST", "/events/{id}/replay", "Alerts & Overlays", "Replay an alert event on overlays.", None, None),
    doc(
        "GET",
        "/events/{id}/why",
        "Alerts & Overlays",
        "Rule trace explaining why an event fired or did not fire.",
        None,
        None,
    ),
    doc("GET", "/goals", "Alerts & Overlays", "List follower, sub, tip, and custom goals.", None, None),
    doc("POST", "/goals", "Alerts & Overlays", "Create a goal.", None, None),
    doc("PUT", "/goals/{id}", "Alerts & Overlays", "Update a goal.", None, None),
    doc("DELETE", "/goals/{id}", "Alerts & Overlays", "Delete a goal.", None, None),
    doc("POST", "/goals/{id}/adjust", "Alerts & Overlays", "Manually adjust a goal's current value.", None, None),
    doc("GET", "/media", "Designs & Media", "List uploaded local alert images, video, and sounds.", None, None),
    doc("POST", "/media", "Designs & Media", "Upload local alert media asset (PC only).", None, None),
    doc("GET", "/media/{id}", "Designs & Media", "Serve a stored media asset by id.", None, None),
    doc("DELETE", "/media/{id}", "Designs & Media", "Delete an uploaded media asset.", None, None),
];

const GO_LIVE_ROUTES: &[RouteDoc] = &[
    doc(
        "GET",
        "/golive",
        "Go Live",
        "Multistream destinations, protection, and vertical canvas configuration.",
        None,
        None,
    ),
    doc("GET", "/golive/ingest", "Go Live", "Local RTMP/SRT ingest URL and stream keys (PC only; secret).", None, None),
    doc(
        "PUT",
        "/golive/destinations",
        "Go Live",
        "Enable, disable, or reorder saved multistream destinations.",
        None,
        None,
    ),
    doc(
        "POST",
        "/golive/destinations",
        "Go Live",
        "Add a custom RTMP/SRT destination with stream key (PC only).",
        None,
        None,
    ),
    doc(
        "DELETE",
        "/golive/destinations/{id}",
        "Go Live",
        "Remove a saved multistream destination (PC only).",
        None,
        None,
    ),
    doc("POST", "/golive/destinations/{id}/test", "Go Live", "Test connectivity to a saved destination.", None, None),
    doc("POST", "/golive/test", "Go Live", "Test an unsaved ingest address (PC only).", None, None),
    doc(
        "GET",
        "/golive/networks",
        "Go Live",
        "Enumerate local network interfaces for RIST bonding (PC only).",
        None,
        None,
    ),
    doc(
        "PUT",
        "/golive/destinations/{id}/light",
        "Go Live",
        "Toggle low-bitrate/light output mode for a destination.",
        None,
        None,
    ),
    doc(
        "PUT",
        "/golive/destinations/{id}/enhanced",
        "Go Live",
        "Toggle Enhanced Broadcasting settings for a destination.",
        None,
        None,
    ),
    doc(
        "POST",
        "/golive/obs/setup",
        "Go Live",
        "Point OBS output at Ferrocast's local multistream ingest.",
        None,
        None,
    ),
    doc("POST", "/golive/obs/restore", "Go Live", "Restore OBS's previous stream settings.", None, None),
    doc("POST", "/golive/upload-check", "Go Live", "Run a short upload bandwidth check.", None, None),
    doc(
        "PUT",
        "/golive/protection",
        "Go Live",
        "Enable/configure stream disconnect protection / BRB fallback.",
        None,
        None,
    ),
    doc("PUT", "/golive/aitum", "Go Live", "Mark Aitum Multistream acknowledgement.", None, None),
    doc("PUT", "/golive/auto-quality", "Go Live", "Configure adaptive bitrate stepping.", None, None),
    doc(
        "POST",
        "/golive/vertical/obs/setup",
        "Go Live",
        "Point Aitum Vertical canvas at Ferrocast ingest.",
        None,
        None,
    ),
    doc("POST", "/golive/vertical/obs/restore", "Go Live", "Restore Aitum Vertical output settings.", None, None),
    doc(
        "PUT",
        "/golive/vertical/source",
        "Go Live",
        "Choose vertical picture source (OBS canvas or cropped landscape).",
        None,
        None,
    ),
    doc("PUT", "/golive/vertical/cut", "Go Live", "Configure cut-from-stream vertical crop layout.", None, None),
    doc("GET", "/golive/picture", "Go Live", "Preview image for setting vertical crop regions.", None, None),
    doc("POST", "/golive/ffmpeg", "Go Live", "Check/fetch local FFmpeg dependency for vertical cutting.", None, None),
    doc(
        "PUT",
        "/golive/advanced",
        "Go Live",
        "Advanced multistream flags (hardware encoder, low latency).",
        None,
        None,
    ),
    doc("POST", "/golive/new-key", "Go Live", "Rotate local multistream ingest key (PC only).", None, None),
    doc("POST", "/golive/resume/{id}", "Go Live", "Reconnect a failed multistream destination.", None, None),
    doc(
        "GET",
        "/stream-info",
        "Go Live",
        "Current stream titles, categories, and presets across connected platforms.",
        None,
        None,
    ),
    doc(
        "POST",
        "/stream-info/apply",
        "Go Live",
        "Push title/category updates to connected streaming platforms.",
        None,
        None,
    ),
    doc("GET", "/stream-info/categories", "Go Live", "Search stream game/topic categories.", None, None),
    doc(
        "GET",
        "/stream-info/categories/as",
        "Go Live",
        "Look up matching category ids across other platforms.",
        None,
        None,
    ),
    doc("POST", "/stream-info/presets", "Go Live", "Save a reusable stream-info title/category preset.", None, None),
    doc("PUT", "/stream-info/presets/{id}", "Go Live", "Update a stream-info preset.", None, None),
    doc("DELETE", "/stream-info/presets/{id}", "Go Live", "Delete a stream-info preset.", None, None),
];

const OBS_AND_MUSIC: &[RouteDoc] = &[
    doc("GET", "/obs", "OBS", "OBS WebSocket connection status (password masked as boolean).", None, None),
    doc("PUT", "/obs", "OBS", "Save manual OBS WebSocket host, port, and password (PC only).", None, None),
    doc(
        "GET",
        "/obs/detect",
        "OBS",
        "Inspect auto-detected OBS settings without exposing password (PC only).",
        None,
        None,
    ),
    doc("POST", "/obs/detect", "OBS", "Switch back to following OBS's auto-detected settings (PC only).", None, None),
    doc("GET", "/obs/theme", "OBS", "Inspect the applied Ferrocast theme inside OBS.", None, None),
    doc("DELETE", "/obs/theme", "OBS", "Remove the Ferrocast theme scenes/sources from OBS.", None, None),
    doc("GET", "/obs/scenes", "OBS", "List OBS scenes and current active scene.", None, None),
    doc("GET", "/obs/inputs", "OBS", "List OBS audio/media inputs and mute states.", None, None),
    doc("POST", "/obs/scene", "OBS", "Switch current OBS program scene.", Some("ObsSceneRequest"), None),
    doc(
        "POST",
        "/obs/source",
        "OBS",
        "Show or hide a source in an OBS scene.",
        Some("ObsSourceVisibilityRequest"),
        None,
    ),
    doc("POST", "/obs/input/mute", "OBS", "Set or toggle mute on an OBS input.", Some("ObsMuteRequest"), None),
    doc("POST", "/obs/replay/save", "OBS", "Save the OBS replay buffer.", None, None),
    doc("POST", "/obs/brb", "OBS", "Toggle Be Right Back scene mode.", None, None),
    doc("GET", "/obs/stream", "OBS", "Get OBS streaming/recording status.", None, None),
    doc("POST", "/obs/stream", "OBS", "Start or stop OBS streaming.", None, None),
    doc("GET", "/obs/health", "OBS", "Get OBS frame drop, CPU, and FPS telemetry.", None, None),
    doc("POST", "/obs/health", "OBS", "Trigger an immediate OBS health snapshot check.", None, None),
    doc("GET", "/obs/switcher", "OBS", "Read automatic scene switcher rules.", None, None),
    doc("PUT", "/obs/switcher", "OBS", "Save automatic scene switcher rules.", None, None),
    doc("GET", "/music", "OBS", "Audio visualizer / now-playing overlay settings.", None, None),
    doc("PUT", "/music", "OBS", "Update audio visualizer / now-playing state.", None, None),
];

const STUDIO_ENGINE_ROUTES: &[RouteDoc] = &[
    doc("GET", "/studio/engine", "Studio Engine", "Built-in Ferrocast studio compositor state.", None, None),
    doc("POST", "/studio/engine/start", "Studio Engine", "Start the studio compositor engine.", None, None),
    doc("POST", "/studio/engine/stop", "Studio Engine", "Stop the studio compositor engine.", None, None),
    doc("GET", "/studio/engine/kinds", "Studio Engine", "List supported source kinds and capabilities.", None, None),
    doc(
        "GET",
        "/studio/engine/kinds/{id}/options",
        "Studio Engine",
        "Inspect configurable option schemas for a source kind.",
        None,
        None,
    ),
    doc("PUT", "/studio/engine/preview", "Studio Engine", "Configure preview surface rectangle/mode.", None, None),
    doc(
        "GET",
        "/studio/engine/kinds/{id}/defaults",
        "Studio Engine",
        "Default properties for a source kind.",
        None,
        None,
    ),
    doc("GET", "/studio/engine/stream", "Studio Engine", "Studio stream output stats and status.", None, None),
    doc("POST", "/studio/engine/stream", "Studio Engine", "Start or stop studio output streaming.", None, None),
    doc("GET", "/studio/engine/scenes", "Studio Engine", "List studio scenes and active scene.", None, None),
    doc(
        "POST",
        "/studio/engine/scenes",
        "Studio Engine",
        "Create a new studio scene.",
        Some("StudioSceneCreate"),
        None,
    ),
    doc("POST", "/studio/engine/vertical", "Studio Engine", "Start vertical canvas output.", None, None),
    doc("DELETE", "/studio/engine/vertical", "Studio Engine", "Stop vertical canvas output.", None, None),
    doc("DELETE", "/studio/engine/scenes/{scene}", "Studio Engine", "Remove a studio scene.", None, None),
    doc("POST", "/studio/engine/scenes/{scene}/duplicate", "Studio Engine", "Duplicate a studio scene.", None, None),
    doc("POST", "/studio/engine/scenes/{scene}/move", "Studio Engine", "Reorder a studio scene.", None, None),
    doc("POST", "/studio/engine/scenes/{scene}/current", "Studio Engine", "Switch the live studio scene.", None, None),
    doc("GET", "/studio/engine/scenes/{scene}/items", "Studio Engine", "List items inside a studio scene.", None, None),
    doc(
        "POST",
        "/studio/engine/scenes/{scene}/items",
        "Studio Engine",
        "Add a source item to a studio scene.",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/scenes/{scene}/items/{id}/edit",
        "Studio Engine",
        "Open or apply item edits.",
        None,
        None,
    ),
    doc(
        "PUT",
        "/studio/engine/scenes/{scene}/items/{id}/box",
        "Studio Engine",
        "Set scene item position and size.",
        Some("StudioItemBox"),
        None,
    ),
    doc(
        "PUT",
        "/studio/engine/scenes/{scene}/items/{id}/crop",
        "Studio Engine",
        "Set scene item edge crop.",
        None,
        None,
    ),
    doc(
        "GET",
        "/studio/engine/scenes/{scene}/thumbnail",
        "Studio Engine",
        "Render a PNG thumbnail of a scene.",
        None,
        None,
    ),
    doc(
        "GET",
        "/studio/engine/scenes/{scene}/items/{id}/transform",
        "Studio Engine",
        "Get item transform, crop, lock, and blend settings.",
        None,
        None,
    ),
    doc(
        "PUT",
        "/studio/engine/scenes/{scene}/items/{id}/transform",
        "Studio Engine",
        "Update item transform or lock state.",
        None,
        None,
    ),
    doc("PUT", "/studio/engine/preview/selection", "Studio Engine", "Select item(s) on the canvas.", None, None),
    doc(
        "PUT",
        "/studio/engine/preview/cursor",
        "Studio Engine",
        "Report preview cursor hover coordinates.",
        None,
        None,
    ),
    doc(
        "GET",
        "/studio/engine/preview/pause",
        "Studio Engine",
        "Check if studio preview rendering is paused.",
        None,
        None,
    ),
    doc(
        "PUT",
        "/studio/engine/preview/pause",
        "Studio Engine",
        "Pause or resume studio preview rendering.",
        None,
        None,
    ),
    doc(
        "GET",
        "/studio/engine/clipboard",
        "Studio Engine",
        "Inspect copied scene item/filter clipboard state.",
        None,
        None,
    ),
    doc("DELETE", "/studio/engine/clipboard", "Studio Engine", "Clear the studio clipboard.", None, None),
    doc("POST", "/studio/engine/clipboard/source", "Studio Engine", "Copy selected scene item.", None, None),
    doc("POST", "/studio/engine/clipboard/filters", "Studio Engine", "Copy source filters.", None, None),
    doc(
        "POST",
        "/studio/engine/clipboard/paste/source",
        "Studio Engine",
        "Paste copied source into a scene.",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/clipboard/paste/filters",
        "Studio Engine",
        "Paste copied filters onto clipboard target.",
        None,
        None,
    ),
    doc("PUT", "/studio/engine/studio-mode", "Studio Engine", "Toggle dual preview/program Studio Mode.", None, None),
    doc("PUT", "/studio/engine/studio-mode/preview", "Studio Engine", "Set Studio Mode preview scene.", None, None),
    doc(
        "POST",
        "/studio/engine/studio-mode/transition",
        "Studio Engine",
        "Transition preview scene to live program.",
        None,
        None,
    ),
    doc(
        "PUT",
        "/studio/engine/studio-mode/options",
        "Studio Engine",
        "Update Studio Mode swap/labels options.",
        None,
        None,
    ),
    doc("POST", "/studio/engine/studio-mode/swap", "Studio Engine", "Swap preview and program scenes.", None, None),
    doc(
        "PUT",
        "/studio/engine/studio-mode/tbar",
        "Studio Engine",
        "Drive the manual transition T-bar position.",
        None,
        None,
    ),
    doc("POST", "/studio/engine/black", "Studio Engine", "Fade program output to black.", None, None),
    doc(
        "PUT",
        "/studio/engine/scenes/{scene}/items/{id}/transition",
        "Studio Engine",
        "Configure show/hide stinger transition for an item.",
        None,
        None,
    ),
    doc(
        "PUT",
        "/studio/engine/scenes/{scene}/items/{id}/group",
        "Studio Engine",
        "Move scene item into/out of a group.",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/scenes/{scene}/items/{id}/ungroup",
        "Studio Engine",
        "Ungroup a folder scene item.",
        None,
        None,
    ),
    doc(
        "PUT",
        "/studio/engine/scenes/{scene}/items/{id}/order",
        "Studio Engine",
        "Reorder scene item z-order.",
        None,
        None,
    ),
    doc(
        "GET",
        "/studio/engine/scenes/{scene}/items/{id}/filters",
        "Studio Engine",
        "List filters attached to an item.",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/scenes/{scene}/items/{id}/filters",
        "Studio Engine",
        "Attach a filter to an item.",
        None,
        None,
    ),
    doc(
        "PUT",
        "/studio/engine/scenes/{scene}/items/{id}/filters/scope",
        "Studio Engine",
        "Set filter scope (shared source vs scene item).",
        None,
        None,
    ),
    doc(
        "DELETE",
        "/studio/engine/scenes/{scene}/items/{id}/filters/{filter}",
        "Studio Engine",
        "Remove a filter from an item.",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/scenes/{scene}/items/{id}/filters/{filter}/edit",
        "Studio Engine",
        "Rename or reorder an item filter.",
        None,
        None,
    ),
    doc(
        "GET",
        "/studio/engine/scenes/{scene}/items/{id}/filters/{filter}/settings",
        "Studio Engine",
        "Read item filter parameter values.",
        None,
        None,
    ),
    doc(
        "PUT",
        "/studio/engine/scenes/{scene}/items/{id}/filters/{filter}/settings",
        "Studio Engine",
        "Update item filter parameter values.",
        None,
        None,
    ),
    doc(
        "GET",
        "/studio/engine/scenes/{scene}/items/{id}/filters/{filter}/options",
        "Studio Engine",
        "Read item filter schema/options.",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/scenes/{scene}/items/{id}/paste-filters",
        "Studio Engine",
        "Paste filters onto a scene item.",
        None,
        None,
    ),
    doc(
        "GET",
        "/studio/engine/scenes/{scene}/items/{id}/picture",
        "Studio Engine",
        "Render a snapshot image of one scene item.",
        None,
        None,
    ),
    doc(
        "PUT",
        "/studio/engine/sources/{name}/deinterlace",
        "Studio Engine",
        "Set source deinterlace mode.",
        None,
        None,
    ),
    doc("PUT", "/studio/engine/quick-transitions", "Studio Engine", "Configure quick transition buttons.", None, None),
    doc("POST", "/studio/engine/undo", "Studio Engine", "Undo the last scene edit.", None, None),
    doc("POST", "/studio/engine/redo", "Studio Engine", "Redo the last undone scene edit.", None, None),
    doc("GET", "/studio/engine/history", "Studio Engine", "Read undo/redo stack summary.", None, None),
    doc(
        "POST",
        "/studio/engine/pick-file",
        "Studio Engine",
        "Open a native OS file picker dialog (PC only).",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/web-window",
        "Studio Engine",
        "Open a URL inside an internal studio window (PC only).",
        None,
        None,
    ),
    doc("GET", "/studio/engine/log", "Studio Engine", "Read engine log path/tail (PC only).", None, None),
    doc(
        "POST",
        "/studio/engine/log/show",
        "Studio Engine",
        "Reveal log file in Finder/Explorer (PC only).",
        None,
        None,
    ),
    doc(
        "GET",
        "/studio/engine/screen-access",
        "Studio Engine",
        "Check macOS Screen Recording/audio permission status.",
        None,
        None,
    ),
    doc(
        "GET",
        "/studio/engine/encode-budget",
        "Studio Engine",
        "Read hardware encoder headroom telemetry.",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/encode-budget",
        "Studio Engine",
        "Estimate encode budget for proposed output settings.",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/screen-access/fix",
        "Studio Engine",
        "Trigger OS screen permission prompt (PC only).",
        None,
        None,
    ),
    doc("GET", "/studio/engine/captions", "Studio Engine", "Live closed-captions settings.", None, None),
    doc("PUT", "/studio/engine/captions", "Studio Engine", "Configure live closed-captions.", None, None),
    doc("GET", "/studio/engine/apps", "Studio Engine", "List capturable application windows.", None, None),
    doc("GET", "/speech/models", "Studio Engine", "List installed/downloadable local speech models.", None, None),
    doc("GET", "/transcript", "Studio Engine", "Search recorded stream captions/transcript.", None, None),
    doc(
        "POST",
        "/speech/models/{id}",
        "Studio Engine",
        "Download a local speech recognition model (PC only).",
        None,
        None,
    ),
    doc("DELETE", "/speech/models/{id}", "Studio Engine", "Delete a downloaded speech model (PC only).", None, None),
    doc(
        "GET",
        "/studio/engine/vertical/follow",
        "Studio Engine",
        "Check if vertical canvas mirrors landscape scene cuts.",
        None,
        None,
    ),
    doc("PUT", "/studio/engine/vertical/follow", "Studio Engine", "Toggle vertical scene follow mode.", None, None),
    doc(
        "POST",
        "/studio/engine/restart",
        "Studio Engine",
        "Restart the studio compositor process (PC only).",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/projectors",
        "Studio Engine",
        "Open a fullscreen or windowed scene projector.",
        None,
        None,
    ),
    doc("DELETE", "/studio/engine/projectors", "Studio Engine", "Close open studio projectors.", None, None),
    doc("POST", "/studio/engine/keys/capture", "Studio Engine", "Capture a hotkey combination (PC only).", None, None),
    doc(
        "POST",
        "/studio/engine/live-alerts/silence",
        "Studio Engine",
        "Silence an active studio health/audio alert.",
        None,
        None,
    ),
    doc("GET", "/studio/engine/screens", "Studio Engine", "List available physical displays.", None, None),
    doc(
        "GET",
        "/studio/engine/library/{kind}",
        "Studio Engine",
        "List saved scene collections or profiles.",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/library/{kind}",
        "Studio Engine",
        "Create a new scene collection or profile.",
        None,
        None,
    ),
    doc(
        "PUT",
        "/studio/engine/library/{kind}/current",
        "Studio Engine",
        "Switch active scene collection or profile.",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/library/{kind}/import",
        "Studio Engine",
        "Import collection/profile file (PC only).",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/library/collections/overlay/preview",
        "Studio Engine",
        "Preview overlay bundle upload (PC only).",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/library/collections/overlay/import",
        "Studio Engine",
        "Import overlay bundle file (PC only).",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/library/collections/overlay/undo",
        "Studio Engine",
        "Undo last overlay bundle import (PC only).",
        None,
        None,
    ),
    doc(
        "DELETE",
        "/studio/engine/library/{kind}/{id}",
        "Studio Engine",
        "Delete a scene collection or profile.",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/library/{kind}/{id}/rename",
        "Studio Engine",
        "Rename a scene collection or profile.",
        None,
        None,
    ),
    doc(
        "GET",
        "/studio/engine/library/{kind}/{id}/export",
        "Studio Engine",
        "Export a collection/profile archive (PC only).",
        None,
        None,
    ),
    doc(
        "GET",
        "/studio/engine/scenes/{scene}/audio",
        "Studio Engine",
        "Audio mixer channels used in a scene.",
        None,
        None,
    ),
    doc(
        "PUT",
        "/studio/engine/transition",
        "Studio Engine",
        "Set global scene transition type and duration.",
        None,
        None,
    ),
    doc(
        "PUT",
        "/studio/engine/top-layer",
        "Studio Engine",
        "Configure pinned top-layer global overlay source.",
        None,
        None,
    ),
    doc(
        "GET",
        "/studio/engine/transitions/{kind}/settings",
        "Studio Engine",
        "Read transition kind settings.",
        None,
        None,
    ),
    doc(
        "PUT",
        "/studio/engine/transitions/{kind}/settings",
        "Studio Engine",
        "Update transition kind settings.",
        None,
        None,
    ),
    doc(
        "PUT",
        "/studio/engine/scenes/{scene}/transition",
        "Studio Engine",
        "Set per-scene transition override.",
        None,
        None,
    ),
    doc(
        "GET",
        "/studio/engine/sources",
        "Studio Engine",
        "List all active studio sources and audio channels.",
        None,
        None,
    ),
    doc("POST", "/studio/engine/sources/{name}/rename", "Studio Engine", "Rename a studio source.", None, None),
    doc("GET", "/studio/engine/sources/{name}/settings", "Studio Engine", "Read source properties.", None, None),
    doc("PUT", "/studio/engine/sources/{name}/settings", "Studio Engine", "Update source properties.", None, None),
    doc(
        "GET",
        "/studio/engine/sources/{name}/options",
        "Studio Engine",
        "Inspect configurable option schemas for a source.",
        None,
        None,
    ),
    doc(
        "PUT",
        "/studio/engine/sources/{name}/audio",
        "Studio Engine",
        "Set audio gain, mute, monitor, sync offset, balance, or tracks.",
        None,
        None,
    ),
    doc("GET", "/studio/engine/sources/{name}/filters", "Studio Engine", "List filters on a source.", None, None),
    doc("POST", "/studio/engine/sources/{name}/filters", "Studio Engine", "Add a filter to a source.", None, None),
    doc(
        "POST",
        "/studio/engine/sources/{name}/record",
        "Studio Engine",
        "Toggle per-source isolated recording.",
        None,
        None,
    ),
    doc("GET", "/studio/guests", "Studio Engine", "List WebRTC/SRT guest slots (PC only).", None, None),
    doc("POST", "/studio/guests/invites", "Studio Engine", "Mint a guest invite URL (PC only).", None, None),
    doc("DELETE", "/studio/guests/invites/{id}", "Studio Engine", "Revoke a guest invite link (PC only).", None, None),
    doc("DELETE", "/studio/guests/{name}", "Studio Engine", "Disconnect an active guest (PC only).", None, None),
    doc(
        "DELETE",
        "/studio/engine/sources/{name}/filters/{filter}",
        "Studio Engine",
        "Remove a filter from a source.",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/sources/{name}/paste-filters",
        "Studio Engine",
        "Paste clipboard filters onto a source.",
        None,
        None,
    ),
    doc("POST", "/studio/engine/sources/{name}/web", "Studio Engine", "Interact with a browser source.", None, None),
    doc(
        "POST",
        "/studio/engine/sources/{name}/screenshot",
        "Studio Engine",
        "Save a PNG screenshot of a source.",
        None,
        None,
    ),
    doc(
        "GET",
        "/studio/engine/sources/{name}/picture",
        "Studio Engine",
        "Fetch a rendered image of a source.",
        None,
        None,
    ),
    doc(
        "GET",
        "/studio/engine/sources/{name}/status",
        "Studio Engine",
        "Playback/health status of a source.",
        None,
        None,
    ),
    doc(
        "GET",
        "/studio/engine/screens/{id}/still",
        "Studio Engine",
        "Capture a static preview frame of a display.",
        None,
        None,
    ),
    doc("GET", "/studio/engine/still", "Studio Engine", "Fetch a static frame thumbnail.", None, None),
    doc("GET", "/studio/engine/vst/plugins", "Studio Engine", "Discover installed VST audio plugins.", None, None),
    doc(
        "GET",
        "/studio/engine/sources/{name}/filters/{filter}/vst",
        "Studio Engine",
        "Inspect VST filter status.",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/sources/{name}/filters/{filter}/vst",
        "Studio Engine",
        "Load/open VST plugin UI.",
        None,
        None,
    ),
    doc(
        "GET",
        "/studio/engine/obs-collections",
        "Studio Engine",
        "Discover local OBS Studio scene collections (PC only).",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/obs-collections/preview",
        "Studio Engine",
        "Preview importing an OBS collection (PC only).",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/obs-collections/import",
        "Studio Engine",
        "Import an OBS scene collection (PC only).",
        None,
        None,
    ),
    doc(
        "GET",
        "/studio/engine/streamlabs-collections",
        "Studio Engine",
        "Discover Streamlabs scene collections (PC only).",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/streamlabs-collections/preview",
        "Studio Engine",
        "Preview importing a Streamlabs collection (PC only).",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/streamlabs-collections/import",
        "Studio Engine",
        "Import a Streamlabs collection (PC only).",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/sources/{name}/filters/{filter}/edit",
        "Studio Engine",
        "Rename, toggle, or reorder a source filter.",
        None,
        None,
    ),
    doc(
        "GET",
        "/studio/engine/sources/{name}/filters/{filter}/settings",
        "Studio Engine",
        "Read source filter parameters.",
        None,
        None,
    ),
    doc(
        "PUT",
        "/studio/engine/sources/{name}/filters/{filter}/settings",
        "Studio Engine",
        "Update source filter parameters.",
        None,
        None,
    ),
    doc(
        "GET",
        "/studio/engine/sources/{name}/filters/{filter}/options",
        "Studio Engine",
        "Read source filter option schema.",
        None,
        None,
    ),
    doc(
        "PUT",
        "/studio/engine/channels/{channel}",
        "Studio Engine",
        "Bind a source to a global audio channel slot.",
        None,
        None,
    ),
    doc(
        "DELETE",
        "/studio/engine/channels/{channel}",
        "Studio Engine",
        "Unbind a global audio channel slot.",
        None,
        None,
    ),
    doc("POST", "/studio/engine/levels", "Studio Engine", "Sample live peak audio levels.", None, None),
    doc("GET", "/studio/engine/audio-care", "Studio Engine", "Audio clipping/ducking care configuration.", None, None),
    doc("PUT", "/studio/engine/audio-care", "Studio Engine", "Save audio care configuration.", None, None),
    doc(
        "GET",
        "/studio/engine/privacy",
        "Studio Engine",
        "Studio privacy guard settings (hide apps/titles).",
        None,
        None,
    ),
    doc("PUT", "/studio/engine/privacy", "Studio Engine", "Save studio privacy guard settings.", None, None),
    doc(
        "GET",
        "/studio/engine/settings",
        "Studio Engine",
        "Read video canvas, FPS, encoder, and output path settings.",
        None,
        None,
    ),
    doc("PUT", "/studio/engine/settings", "Studio Engine", "Save video/encoder/audio settings.", None, None),
    doc(
        "GET",
        "/studio/engine/stats",
        "Studio Engine",
        "Compositor CPU, GPU, frame-time, and dropped-frame stats.",
        None,
        None,
    ),
    doc("POST", "/studio/engine/record", "Studio Engine", "Start or stop local recording.", None, None),
    doc("POST", "/studio/engine/record/pause", "Studio Engine", "Pause or resume local recording.", None, None),
    doc(
        "POST",
        "/studio/engine/record/split",
        "Studio Engine",
        "Split active recording into a new segment file.",
        None,
        None,
    ),
    doc("POST", "/studio/engine/replay", "Studio Engine", "Enable or disable the replay buffer.", None, None),
    doc(
        "GET",
        "/studio/engine/missing-files",
        "Studio Engine",
        "List missing asset paths referenced in scenes (PC only).",
        None,
        None,
    ),
    doc(
        "PUT",
        "/studio/engine/missing-files",
        "Studio Engine",
        "Relink a missing asset file path (PC only).",
        None,
        None,
    ),
    doc(
        "GET",
        "/studio/engine/sources/{name}/media",
        "Studio Engine",
        "Read playback cursor and duration for a media source.",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/sources/{name}/media",
        "Studio Engine",
        "Trigger play, pause, restart, or seek on a media source.",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/missing-files/search",
        "Studio Engine",
        "Search local folders for missing files (PC only).",
        None,
        None,
    ),
    doc("POST", "/studio/engine/replay/save", "Studio Engine", "Save replay buffer clip to disk.", None, None),
    doc("POST", "/studio/engine/virtualcam", "Studio Engine", "Start or stop Virtual Camera output.", None, None),
    doc(
        "GET",
        "/studio/engine/virtualcam/setup",
        "Studio Engine",
        "Check virtual camera driver installation status.",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/virtualcam/setup",
        "Studio Engine",
        "Install or configure virtual camera driver.",
        None,
        None,
    ),
    doc("POST", "/studio/engine/ndi", "Studio Engine", "Start or stop NDI network output.", None, None),
    doc(
        "POST",
        "/studio/engine/record/chapter",
        "Studio Engine",
        "Insert a chapter marker in the current recording.",
        None,
        None,
    ),
    doc("POST", "/studio/engine/remux", "Studio Engine", "Remux an MKV recording to MP4 (PC only).", None, None),
    doc(
        "GET",
        "/studio/remote-control",
        "Studio Engine",
        "Inspect local obs-websocket server status (PC only).",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/remote-control",
        "Studio Engine",
        "Configure/enable local obs-websocket server (PC only).",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/remote-control/password",
        "Studio Engine",
        "Rotate obs-websocket server password (PC only).",
        None,
        None,
    ),
    doc(
        "POST",
        "/studio/engine/encoders/{id}/options",
        "Studio Engine",
        "Query encoder options for a hardware backend.",
        None,
        None,
    ),
    doc(
        "GET",
        "/studio/engine/monitoring",
        "Studio Engine",
        "List available audio monitoring output devices.",
        None,
        None,
    ),
];

const MOMENTS_AND_SHOW: &[RouteDoc] = &[
    doc("GET", "/moments/config", "Moments & Show", "Moments/clippable-highlight detection configuration.", None, None),
    doc("PUT", "/moments/config", "Moments & Show", "Save Moments detection and chime settings.", None, None),
    doc("GET", "/moments/lane", "Moments & Show", "List active live Moments lane items.", None, None),
    doc("POST", "/moments/lane/dismiss-all", "Moments & Show", "Dismiss all items in the Moments lane.", None, None),
    doc("POST", "/moments/lane/{id}/dismiss", "Moments & Show", "Dismiss one item from the Moments lane.", None, None),
    doc("GET", "/moments/markers", "Moments & Show", "List recorded stream timeline markers.", None, None),
    doc(
        "POST",
        "/moments/markers",
        "Moments & Show",
        "Drop a timestamped stream marker.",
        Some("MomentMarkerCreate"),
        None,
    ),
    doc(
        "GET",
        "/moments/markers/export",
        "Moments & Show",
        "Export stream markers (Markdown/YouTube chapters/CSV).",
        None,
        None,
    ),
    doc("DELETE", "/moments/markers/{id}", "Moments & Show", "Delete a stream marker.", None, None),
    doc("GET", "/show", "Moments & Show", "Active stream show rundown state and preflight.", None, None),
    doc("GET", "/show/config", "Moments & Show", "Read show startup checklist & countdown configuration.", None, None),
    doc("PUT", "/show/config", "Moments & Show", "Update show checklist and countdown configuration.", None, None),
    doc("GET", "/show/preflight", "Moments & Show", "Run preflight checks before going live.", None, None),
    doc("GET", "/show/alerts", "Moments & Show", "Active live show health/warning banners.", None, None),
    doc(
        "POST",
        "/show/preflight/fix",
        "Moments & Show",
        "Attempt automatic fix for a failing preflight check.",
        None,
        None,
    ),
    doc(
        "POST",
        "/show/start",
        "Moments & Show",
        "Begin go-live countdown/show rundown.",
        Some("ShowStartRequest"),
        None,
    ),
    doc("POST", "/show/countdown/skip", "Moments & Show", "Skip countdown and go live immediately.", None, None),
    doc("POST", "/show/countdown/cancel", "Moments & Show", "Cancel the pending go-live countdown.", None, None),
    doc("POST", "/show/live-scene", "Moments & Show", "Cut from pre-show to live main scene.", None, None),
    doc("POST", "/show/end", "Moments & Show", "End the active show rundown cleanly.", None, None),
    doc("GET", "/automations", "Automations", "List trigger/action automation rules.", None, None),
    doc("PUT", "/automations", "Automations", "Save the full list of automation rules.", None, None),
    doc("POST", "/automations/{id}/run", "Automations", "Manually test-run one automation rule.", None, None),
];

const SETTINGS_AND_IMPORTS: &[RouteDoc] = &[
    doc("GET", "/settings", "Settings & Backups", "Read Ferrocast application settings.", None, None),
    doc(
        "PUT",
        "/settings",
        "Settings & Backups",
        "Patch settings (API keys cannot flip lan_access).",
        Some("SettingsPatch"),
        None,
    ),
    doc("GET", "/updates", "Settings & Backups", "Current app update check/download status.", None, None),
    doc("POST", "/updates/check", "Settings & Backups", "Check update feed for newer releases (PC only).", None, None),
    doc(
        "POST",
        "/updates/install",
        "Settings & Backups",
        "Download and verify signed update installer (PC only).",
        None,
        None,
    ),
    doc("POST", "/updates/restart", "Settings & Backups", "Restart Ferrocast to apply update (PC only).", None, None),
    doc("POST", "/setup/complete", "Settings & Backups", "Mark first-run onboarding as complete.", None, None),
    doc("GET", "/backups", "Settings & Backups", "List local SQLite backup snapshots.", None, None),
    doc("POST", "/backups", "Settings & Backups", "Create a manual local database snapshot.", None, None),
    doc(
        "POST",
        "/backups/{id}/restore",
        "Settings & Backups",
        "Schedule database snapshot restore on restart (PC only).",
        None,
        None,
    ),
    doc("POST", "/export", "Imports & Exports", "Export portable `.ferropack` backup bundle (PC only).", None, None),
    doc("POST", "/import", "Imports & Exports", "Import a `.ferropack` backup bundle (PC only).", None, None),
    doc(
        "POST",
        "/import/streamelements/preview",
        "Imports & Exports",
        "Preview StreamElements migration using one-shot token (PC only).",
        None,
        None,
    ),
    doc(
        "POST",
        "/import/streamlabs/preview",
        "Imports & Exports",
        "Preview Streamlabs CSV/XLSX tips and points upload (PC only).",
        None,
        None,
    ),
    doc("POST", "/import/apply", "Imports & Exports", "Apply a previewed third-party migration (PC only).", None, None),
];

const ACCOUNTS_AND_PRO: &[RouteDoc] = &[
    doc("GET", "/accounts", "Accounts & Pro", "Connected platform account statuses (PC only).", None, None),
    doc(
        "POST",
        "/accounts/{platform}/connect",
        "Accounts & Pro",
        "Connect or refresh a platform account via OAuth/code (PC only).",
        None,
        None,
    ),
    doc("DELETE", "/accounts/{platform}", "Accounts & Pro", "Disconnect a platform account (PC only).", None, None),
    doc("GET", "/tips", "Accounts & Pro", "Tip page configuration (PC only).", None, None),
    doc("PUT", "/tips", "Accounts & Pro", "Save tip page & gateway credentials (PC only).", None, None),
    doc("DELETE", "/tips", "Accounts & Pro", "Take down tip page configuration (PC only).", None, None),
    doc("GET", "/tips/brand", "Accounts & Pro", "Tip page custom brand colors and assets (PC only).", None, None),
    doc("PUT", "/tips/brand", "Accounts & Pro", "Update tip page brand settings (PC only).", None, None),
    doc("DELETE", "/tips/brand", "Accounts & Pro", "Reset tip page brand settings (PC only).", None, None),
    doc("PUT", "/tips/brand/{kind}", "Accounts & Pro", "Upload tip page avatar or banner (PC only).", None, None),
    doc("DELETE", "/tips/brand/{kind}", "Accounts & Pro", "Delete tip page avatar or banner (PC only).", None, None),
    doc("GET", "/tips/goal", "Accounts & Pro", "Tip page progress goal (PC only).", None, None),
    doc("PUT", "/tips/goal", "Accounts & Pro", "Save tip page progress goal (PC only).", None, None),
    doc("DELETE", "/tips/goal", "Accounts & Pro", "Delete tip page progress goal (PC only).", None, None),
    doc("GET", "/discord", "Accounts & Pro", "Discord go-live webhook configuration (PC only).", None, None),
    doc("PUT", "/discord", "Accounts & Pro", "Save Discord webhook URL and template (PC only).", None, None),
    doc("POST", "/discord/test", "Accounts & Pro", "Send a test Discord webhook notification (PC only).", None, None),
    doc("GET", "/pro", "Accounts & Pro", "Ferrocast Pro subscription state (PC only).", None, None),
    doc("POST", "/pro/refresh", "Accounts & Pro", "Refresh Pro entitlement state (PC only).", None, None),
    doc("POST", "/pro/checkout", "Accounts & Pro", "Create Pro checkout link (PC only).", None, None),
    doc("POST", "/pro/manage", "Accounts & Pro", "Open Pro billing portal link (PC only).", None, None),
    doc("POST", "/pro/cancel", "Accounts & Pro", "Cancel Pro subscription (PC only).", None, None),
    doc("POST", "/pro/restore", "Accounts & Pro", "Restore Pro purchase (PC only).", None, None),
    doc("POST", "/pro/redeem", "Accounts & Pro", "Redeem a Pro promo/gift key (PC only).", None, None),
    doc("GET", "/pro/account", "Accounts & Pro", "Ferrocast cloud account sign-in state (PC only).", None, None),
    doc("POST", "/pro/account/sign-in", "Accounts & Pro", "Start cloud sign-in flow (PC only).", None, None),
    doc("POST", "/pro/account/sign-in/cancel", "Accounts & Pro", "Cancel pending cloud sign-in (PC only).", None, None),
    doc(
        "POST",
        "/pro/account/sign-out",
        "Accounts & Pro",
        "Sign out of Ferrocast cloud account (PC only).",
        None,
        None,
    ),
    doc("GET", "/pro/account/sessions", "Accounts & Pro", "Active cloud account sessions (PC only).", None, None),
    doc("DELETE", "/pro/account/sessions/{id}", "Accounts & Pro", "Revoke one cloud session (PC only).", None, None),
    doc("GET", "/pro/remote", "Accounts & Pro", "Paired remote phone controllers via relay (PC only).", None, None),
    doc("POST", "/pro/remote/codes", "Accounts & Pro", "Mint remote controller pairing code (PC only).", None, None),
    doc(
        "DELETE",
        "/pro/remote/devices/{id}",
        "Accounts & Pro",
        "Revoke a paired remote controller (PC only).",
        None,
        None,
    ),
    doc("GET", "/pro/cloud", "Accounts & Pro", "Cloud sync status (PC only).", None, None),
    doc("PUT", "/pro/cloud", "Accounts & Pro", "Toggle cloud sync (PC only).", None, None),
    doc("POST", "/pro/cloud/sync", "Accounts & Pro", "Trigger manual cloud sync (PC only).", None, None),
    doc("GET", "/pro/history", "Accounts & Pro", "Cloud multi-stream analytics history (PC only).", None, None),
    doc("GET", "/pro/history/export.csv", "Accounts & Pro", "Export cloud analytics as CSV (PC only).", None, None),
    doc(
        "GET",
        "/pro/history/samples",
        "Accounts & Pro",
        "Sample time-series points for one stream (PC only).",
        None,
        None,
    ),
    doc("GET", "/pro/history/email", "Accounts & Pro", "Weekly email digest settings (PC only).", None, None),
    doc("PUT", "/pro/history/email", "Accounts & Pro", "Save weekly email digest settings (PC only).", None, None),
    doc("DELETE", "/pro/history/email", "Accounts & Pro", "Stop weekly email digest (PC only).", None, None),
    doc("GET", "/pro/smart", "Accounts & Pro", "Smart Moderation & AI co-host settings (PC only).", None, None),
    doc("PUT", "/pro/smart", "Accounts & Pro", "Update Smart Moderation settings (PC only).", None, None),
    doc(
        "POST",
        "/pro/smart/about/approve",
        "Accounts & Pro",
        "Approve creator context profile for AI prompts (PC only).",
        None,
        None,
    ),
    doc("GET", "/pro/smart/moderation", "Accounts & Pro", "Smart Moderation review queue (PC only).", None, None),
    doc(
        "POST",
        "/pro/smart/moderation/{id}/apply",
        "Accounts & Pro",
        "Apply suggested Smart Moderation action (PC only).",
        None,
        None,
    ),
    doc(
        "POST",
        "/pro/smart/moderation/{id}/undo",
        "Accounts & Pro",
        "Undo applied Smart Moderation action (PC only).",
        None,
        None,
    ),
    doc(
        "POST",
        "/pro/smart/moderation/{id}/dismiss",
        "Accounts & Pro",
        "Dismiss Smart Moderation suggestion (PC only).",
        None,
        None,
    ),
    doc("GET", "/pro/bot", "Accounts & Pro", "Hosted Always-On Cloud Bot state (PC only).", None, None),
    doc("PUT", "/pro/bot/enabled", "Accounts & Pro", "Enable or disable Always-On Cloud Bot (PC only).", None, None),
    doc(
        "POST",
        "/pro/bot/{platform}/connect",
        "Accounts & Pro",
        "Connect bot to platform via relay (PC only).",
        None,
        None,
    ),
    doc("DELETE", "/pro/bot/{platform}", "Accounts & Pro", "Disconnect cloud bot from platform (PC only).", None, None),
    doc("GET", "/pro/discord", "Accounts & Pro", "Managed Discord bot integration status (PC only).", None, None),
    doc("DELETE", "/pro/discord", "Accounts & Pro", "Disconnect managed Discord bot (PC only).", None, None),
    doc(
        "POST",
        "/pro/discord/connect",
        "Accounts & Pro",
        "Connect Discord server to managed bot (PC only).",
        None,
        None,
    ),
    doc("PUT", "/pro/discord/channel", "Accounts & Pro", "Select Discord announcement channel (PC only).", None, None),
    doc("PUT", "/pro/discord/go-live", "Accounts & Pro", "Configure Discord bot go-live posts (PC only).", None, None),
    doc("PUT", "/pro/discord/rules", "Accounts & Pro", "Add Discord role/reward rule (PC only).", None, None),
    doc("PUT", "/pro/discord/rules/{id}", "Accounts & Pro", "Update Discord rule (PC only).", None, None),
    doc("DELETE", "/pro/discord/rules/{id}", "Accounts & Pro", "Delete Discord rule (PC only).", None, None),
    doc("GET", "/pro/backups", "Accounts & Pro", "List encrypted cloud backup archives (PC only).", None, None),
    doc("POST", "/pro/backups", "Accounts & Pro", "Upload an encrypted cloud backup now (PC only).", None, None),
    doc("GET", "/pro/backups/key", "Accounts & Pro", "Inspect recovery key state (PC only).", None, None),
    doc("POST", "/pro/backups/key/saved", "Accounts & Pro", "Confirm recovery key saved (PC only).", None, None),
    doc("PUT", "/pro/backups/passphrase", "Accounts & Pro", "Set cloud backup passphrase (PC only).", None, None),
    doc("DELETE", "/pro/backups/passphrase", "Accounts & Pro", "Remove cloud backup passphrase (PC only).", None, None),
    doc(
        "PUT",
        "/pro/backups/settings",
        "Accounts & Pro",
        "Save encrypted cloud backup schedule (PC only).",
        None,
        None,
    ),
    doc("DELETE", "/pro/backups/{id}", "Accounts & Pro", "Delete one cloud backup snapshot (PC only).", None, None),
    doc(
        "POST",
        "/pro/backups/{id}/restore",
        "Accounts & Pro",
        "Restore an encrypted cloud backup (PC only).",
        None,
        None,
    ),
    doc("GET", "/pro/tips/domain", "Accounts & Pro", "Custom tip domain verification state (PC only).", None, None),
    doc("PUT", "/pro/tips/domain", "Accounts & Pro", "Configure custom tip page domain (PC only).", None, None),
    doc("DELETE", "/pro/tips/domain", "Accounts & Pro", "Remove custom tip page domain (PC only).", None, None),
];

const PLUGINS_AND_DIAGNOSTICS: &[RouteDoc] = &[
    doc("GET", "/plugins", "Plugins", "List installed sandboxed WASM/panel plugins (PC only).", None, None),
    doc("POST", "/plugins", "Plugins", "Install a `.ferroplugin` package file (PC only).", None, None),
    doc("GET", "/plugins/catalog", "Plugins", "Browse signed plugin catalog (PC only).", None, None),
    doc(
        "POST",
        "/plugins/catalog/install",
        "Plugins",
        "Install a signed plugin from the catalog (PC only).",
        None,
        None,
    ),
    doc("PUT", "/plugins/{id}", "Plugins", "Enable or disable an installed plugin (PC only).", None, None),
    doc("DELETE", "/plugins/{id}", "Plugins", "Uninstall a plugin (PC only).", None, None),
    doc("GET", "/plugins/{id}/panel", "Plugins", "Inspect plugin panel metadata (PC only).", None, None),
    doc("POST", "/plugins/{id}/act", "Plugins", "Dispatch an action to a plugin (PC only).", None, None),
    doc(
        "GET",
        "/diagnostics/preview",
        "Diagnostics",
        "Inspect redacted diagnostic report preview (PC only).",
        None,
        None,
    ),
    doc(
        "POST",
        "/diagnostics/report",
        "Diagnostics",
        "Write redacted diagnostic report JSON to disk (PC only).",
        None,
        None,
    ),
    doc("POST", "/diagnostics/report.zip", "Diagnostics", "Build support report zip archive (PC only).", None, None),
];

/// Every HTTP REST route registered on `/api/v1`.
pub fn all_rest_routes() -> impl Iterator<Item = &'static RouteDoc> {
    CORE_AND_KEYS
        .iter()
        .chain(CHAT_AND_MOD)
        .chain(BOT_AND_WIDGETS)
        .chain(ALERTS_DESIGNS_MEDIA)
        .chain(GO_LIVE_ROUTES)
        .chain(OBS_AND_MUSIC)
        .chain(STUDIO_ENGINE_ROUTES)
        .chain(MOMENTS_AND_SHOW)
        .chain(SETTINGS_AND_IMPORTS)
        .chain(ACCOUNTS_AND_PRO)
        .chain(PLUGINS_AND_DIAGNOSTICS)
}

static OPENAPI: LazyLock<Value> = LazyLock::new(build_openapi);
static ASYNCAPI: LazyLock<Value> = LazyLock::new(build_asyncapi);

/// Returns the cached OpenAPI 3.1.0 specification document.
#[must_use]
pub fn openapi() -> &'static Value {
    &OPENAPI
}

/// Returns the cached AsyncAPI 3.0.0 specification document.
#[must_use]
pub fn asyncapi() -> &'static Value {
    &ASYNCAPI
}

/// Serializes the cached OpenAPI 3.1.0 specification as pretty-printed JSON.
pub fn openapi_pretty() -> Result<String, serde_json::Error> {
    let mut out = serde_json::to_string_pretty(openapi())?;
    out.push('\n');
    Ok(out)
}

/// Serializes the cached AsyncAPI 3.0.0 specification as pretty-printed JSON.
pub fn asyncapi_pretty() -> Result<String, serde_json::Error> {
    let mut out = serde_json::to_string_pretty(asyncapi())?;
    out.push('\n');
    Ok(out)
}

/// `GET /api/v1/openapi.json`
pub async fn openapi_json() -> Json<&'static Value> {
    Json(openapi())
}

/// `GET /api/v1/asyncapi.json`
pub async fn asyncapi_json() -> Json<&'static Value> {
    Json(asyncapi())
}

fn path_params(rel_path: &str) -> Vec<Value> {
    rel_path
        .split('/')
        .filter_map(|seg| seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
        .map(|name| {
            json!({
                "name": name,
                "in": "path",
                "required": true,
                "schema": { "type": "string" }
            })
        })
        .collect()
}

fn operation_id(method: &str, rel_path: &str) -> String {
    let mut id = method.to_lowercase();
    for seg in rel_path.trim_start_matches('/').split('/') {
        id.push('_');
        for ch in seg.chars() {
            if ch.is_ascii_alphanumeric() {
                id.push(ch);
            } else {
                id.push('_');
            }
        }
    }
    id
}

fn allowed_key_scopes_for(method: &str, rel_path: &str) -> Vec<&'static str> {
    let probe = sample_path(rel_path);
    ApiKeyScope::ALL
        .iter()
        .copied()
        .filter(|s| remote_scope::api_key_allowed(&[*s], method, &probe))
        .map(ApiKeyScope::as_str)
        .collect()
}

fn sample_path(rel_path: &str) -> String {
    let segs: Vec<&str> = rel_path
        .trim_start_matches('/')
        .split('/')
        .map(|s| if s.starts_with('{') && s.ends_with('}') { "sample" } else { s })
        .collect();
    format!("/{}", segs.join("/"))
}

fn build_openapi() -> Value {
    let mut paths: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    for r in all_rest_routes() {
        let full_path = format!("/api/v1{}", r.rel_path);
        let method_key = r.method.to_lowercase();
        let scopes = allowed_key_scopes_for(r.method, r.rel_path);
        let pc_only = scopes.is_empty() && r.rel_path != "/session/pair";

        let mut op = Map::new();
        op.insert("tags".into(), json!([r.tag]));
        op.insert("summary".into(), json!(r.summary));
        op.insert("operationId".into(), json!(operation_id(r.method, r.rel_path)));
        op.insert("x-ferrocast-scopes".into(), json!(scopes));
        if pc_only {
            op.insert("x-pc-only".into(), json!(true));
        }

        let params = path_params(r.rel_path);
        if !params.is_empty() {
            op.insert("parameters".into(), Value::Array(params));
        }

        if r.rel_path == "/session/pair" {
            op.insert("security".into(), json!([]));
        } else {
            op.insert("security".into(), json!([{ "bearerAuth": [] }]));
        }

        if let Some(req_ref) = r.req_schema {
            op.insert(
                "requestBody".into(),
                json!({
                    "required": true,
                    "content": {
                        "application/json": {
                            "schema": { "$ref": format!("#/components/schemas/{req_ref}") }
                        }
                    }
                }),
            );
        } else if matches!(r.method, "POST" | "PUT") {
            op.insert(
                "requestBody".into(),
                json!({
                    "content": {
                        "application/json": { "schema": { "type": "object" } }
                    }
                }),
            );
        }

        let ok_status = match r.method {
            "POST" if r.rel_path == "/api-keys" => "201",
            "DELETE" => "204",
            _ => "200",
        };
        let ok_entry = if ok_status == "204" {
            json!({ "description": "Success (no content)" })
        } else if let Some(resp_ref) = r.resp_schema {
            json!({
                "description": "Success",
                "content": {
                    "application/json": {
                        "schema": { "$ref": format!("#/components/schemas/{resp_ref}") }
                    }
                }
            })
        } else {
            json!({
                "description": "Success",
                "content": {
                    "application/json": { "schema": { "type": ["object", "array"] } }
                }
            })
        };

        let responses = json!({
            ok_status: ok_entry,
            "401": { "description": "Missing, invalid, revoked, or expired credentials.", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/ApiError" } } } },
            "403": { "description": "Insufficient API key scope, forbidden origin, or PC-only endpoint.", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/ApiError" } } } },
            "429": { "description": "Per-key rate limit exceeded (see Retry-After header).", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/ApiError" } } } }
        });
        op.insert("responses".into(), responses);

        paths.entry(full_path).or_default().insert(method_key, Value::Object(op));
    }

    let mut path_obj = Map::new();
    for (p, ops) in paths {
        path_obj.insert(p, Value::Object(ops));
    }

    json!({
        "openapi": "3.1.0",
        "jsonSchemaDialect": "https://spec.openapis.org/oas/3.1/dialect/base",
        "info": {
            "title": "Ferrocast Local REST API",
            "version": env!("CARGO_PKG_VERSION"),
            "summary": "Loopback HTTP API exposed by the local Ferrocast engine and desktop daemon.",
            "description": "Authenticate using a dashboard pairing token or a scoped local API key (`Authorization: Bearer fc_key_...`). Keys are hashed with SHA-256, rate-limited (120 req/min default), audited without storing the secret, and never permitted on PC-only secret, account, plugin, diagnostics, or key-management endpoints."
        },
        "servers": [
            { "url": "http://127.0.0.1:47820", "description": "Local Ferrocast daemon (loopback default)" }
        ],
        "components": {
            "securitySchemes": {
                "bearerAuth": {
                    "type": "http",
                    "scheme": "bearer",
                    "bearerFormat": "fc_key_<64 hex> or session token",
                    "description": "Scoped local API key (`fc_key_...`) or browser session token from `POST /api/v1/session/pair`."
                }
            },
            "schemas": schemas()
        },
        "paths": Value::Object(path_obj)
    })
}

fn schemas() -> Value {
    json!({
        "ApiError": {
            "type": "object",
            "required": ["error"],
            "properties": {
                "error": {
                    "type": "object",
                    "required": ["code", "message"],
                    "properties": {
                        "code": { "type": "string" },
                        "message": { "type": "string" },
                        "hint": { "type": ["string", "null"] }
                    }
                }
            }
        },
        "ApiKeyScope": {
            "type": "string",
            "enum": ["read_only", "chat", "alerts", "studio_control", "go_live", "settings"]
        },
        "ApiKeyCreate": {
            "type": "object",
            "required": ["name", "scopes"],
            "properties": {
                "name": { "type": "string", "minLength": 1, "maxLength": 60 },
                "scopes": {
                    "type": "array",
                    "minItems": 1,
                    "items": { "$ref": "#/components/schemas/ApiKeyScope" }
                },
                "expires_at": { "type": ["integer", "null"], "description": "Optional Unix millisecond expiration timestamp." }
            }
        },
        "ApiKeySummary": {
            "type": "object",
            "required": ["id", "name", "prefix", "scopes", "created_at"],
            "properties": {
                "id": { "type": "string" },
                "name": { "type": "string" },
                "prefix": { "type": "string" },
                "scopes": { "type": "array", "items": { "$ref": "#/components/schemas/ApiKeyScope" } },
                "created_at": { "type": "integer" },
                "expires_at": { "type": ["integer", "null"] },
                "last_used_at": { "type": ["integer", "null"] },
                "revoked_at": { "type": ["integer", "null"] }
            }
        },
        "ApiKeyCreated": {
            "type": "object",
            "required": ["key", "token"],
            "properties": {
                "key": { "$ref": "#/components/schemas/ApiKeySummary" },
                "token": { "type": "string", "description": "Plain secret shown once at creation (`fc_key_<64 hex>`). Never stored or returned again." }
            }
        },
        "ApiKeyAuditEntry": {
            "type": "object",
            "required": ["id", "key_id", "key_name", "method", "path", "status", "at"],
            "properties": {
                "id": { "type": "string" },
                "key_id": { "type": "string" },
                "key_name": { "type": "string" },
                "method": { "type": "string" },
                "path": { "type": "string" },
                "status": { "type": "integer" },
                "at": { "type": "integer" }
            }
        },
        "ApiKeysView": {
            "type": "object",
            "required": ["keys", "audit"],
            "properties": {
                "keys": { "type": "array", "items": { "$ref": "#/components/schemas/ApiKeySummary" } },
                "audit": { "type": "array", "items": { "$ref": "#/components/schemas/ApiKeyAuditEntry" } }
            }
        },
        "StatusResponse": {
            "type": "object",
            "required": ["version", "port", "connectors"],
            "properties": {
                "version": { "type": "string" },
                "port": { "type": "integer" },
                "setup_done": { "type": "boolean" },
                "connectors": { "type": "object" }
            }
        },
        "PairRequest": {
            "type": "object",
            "required": ["code"],
            "properties": { "code": { "type": "string" } }
        },
        "PairResponse": {
            "type": "object",
            "required": ["token"],
            "properties": { "token": { "type": "string" } }
        },
        "ChatSendRequest": {
            "type": "object",
            "required": ["text", "platforms"],
            "properties": {
                "platforms": {
                    "oneOf": [
                        { "type": "string", "description": "Either 'all' or a single platform id (e.g. 'twitch')." },
                        { "type": "array", "items": { "type": "string" } }
                    ]
                },
                "text": { "type": "string", "maxLength": 2000 },
                "reply_to": {
                    "type": ["object", "null"],
                    "properties": {
                        "platform": { "type": "string" },
                        "message_id": { "type": "string" }
                    }
                }
            }
        },
        "ModTimeoutRequest": {
            "type": "object",
            "required": ["platform", "user_id", "duration_s"],
            "properties": {
                "platform": { "type": "string" },
                "user_id": { "type": "string" },
                "duration_s": { "type": "integer" },
                "reason": { "type": ["string", "null"] }
            }
        },
        "ModBanRequest": {
            "type": "object",
            "required": ["platform", "user_id"],
            "properties": {
                "platform": { "type": "string" },
                "user_id": { "type": "string" },
                "reason": { "type": ["string", "null"] }
            }
        },
        "TestAlertRequest": {
            "type": "object",
            "required": ["kind"],
            "properties": {
                "kind": { "type": "string" },
                "platform": { "type": "string" },
                "user": { "type": "string" },
                "amount": { "type": "number" },
                "message": { "type": "string" }
            }
        },
        "ObsSceneRequest": {
            "type": "object",
            "required": ["scene"],
            "properties": { "scene": { "type": "string" } }
        },
        "ObsSourceVisibilityRequest": {
            "type": "object",
            "required": ["scene", "item_id", "visible"],
            "properties": {
                "scene": { "type": "string" },
                "item_id": { "type": "integer" },
                "visible": { "type": "boolean" }
            }
        },
        "ObsMuteRequest": {
            "type": "object",
            "required": ["input", "muted"],
            "properties": {
                "input": { "type": "string" },
                "muted": { "type": "boolean" }
            }
        },
        "StudioSceneCreate": {
            "type": "object",
            "required": ["name"],
            "properties": { "name": { "type": "string" } }
        },
        "StudioItemBox": {
            "type": "object",
            "required": ["x", "y", "w", "h"],
            "properties": {
                "x": { "type": "number" },
                "y": { "type": "number" },
                "w": { "type": "number" },
                "h": { "type": "number" }
            }
        },
        "MomentMarkerCreate": {
            "type": "object",
            "required": ["label"],
            "properties": {
                "label": { "type": "string" },
                "kind": { "type": "string" }
            }
        },
        "ShowStartRequest": {
            "type": "object",
            "properties": {
                "countdown_s": { "type": "integer" }
            }
        },
        "SettingsPatch": {
            "type": "object",
            "description": "Partial update of non-sensitive local application preferences."
        }
    })
}

fn live_message(summary: &str, scope_note: &str, data_schema: Value) -> Value {
    json!({
        "summary": summary,
        "x-ferrocast-scopes": scope_note,
        "payload": {
            "type": "object",
            "required": ["v", "type", "data"],
            "properties": {
                "v": { "type": "integer", "const": 1 },
                "type": { "type": "string" },
                "seq": { "type": ["integer", "null"] },
                "data": data_schema
            }
        }
    })
}

fn build_asyncapi() -> Value {
    json!({
        "asyncapi": "3.0.0",
        "info": {
            "title": "Ferrocast Live & Overlay WebSocket API",
            "version": env!("CARGO_PKG_VERSION"),
            "summary": "Real-time event, queue, status, studio telemetry, and browser-source overlay streams.",
            "description": "Clients authenticate on `/api/v1/live` using `Authorization: Bearer fc_key_...` (recommended), `?key=...`, or `?session=...`. Reconnecting with `?since=<seq>` replays buffered frames in order. Overlay browser sources connect to `/o/{token}/ws`."
        },
        "servers": {
            "local": {
                "host": "127.0.0.1:47820",
                "protocol": "ws",
                "description": "Ferrocast local WebSocket server (loopback default)"
            }
        },
        "channels": {
            "live": {
                "address": "/api/v1/live",
                "description": "Live multiplexed dashboard & API integration stream with sequence replay (`?since=seq`). API keys receive only frame types allowed by their scopes and never receive `account` frames.",
                "parameters": {
                    "since": { "description": "Optional last processed `seq` for lossless reconnect replay." },
                    "key": { "description": "API key when header auth isn't available (prefer `Authorization: Bearer fc_key_...`)." },
                    "session": { "description": "Browser session token." }
                },
                "messages": {
                    "hello": live_message("Initial handshake with highest sequence number (`{ seq }`).", "read_only, chat, alerts, studio_control, go_live", json!({ "type": "object", "properties": { "seq": { "type": "integer" } } })),
                    "resync": live_message("Ring buffer gap notice (`{ missed }`); clients should re-fetch state over REST.", "read_only, chat, alerts, studio_control, go_live", json!({ "type": "object", "properties": { "missed": { "type": "integer" } } })),
                    "status": live_message("Connector and OBS health snapshots.", "read_only, chat, alerts, studio_control, go_live", json!({ "type": "object", "properties": { "connectors": { "type": "object" } } })),
                    "queue": live_message("Alert queue snapshot (active/paused/muted/pending count).", "read_only, alerts", json!({ "type": "object" })),
                    "event": live_message("Normalized stream event (follow, sub, gift_sub, tip, bits, raid, chat_message, etc.).", "read_only; chat (chat/mod kinds); alerts (stream events)", json!({ "type": "object", "properties": { "id": { "type": "string" }, "platform": { "type": "string" }, "kind": { "type": "object" } } })),
                    "notice": live_message("Engine status notice or panic warning (`account:*` notices suppressed for API keys).", "read_only, alerts, studio_control, go_live (chat sees panic only)", json!({ "type": "object", "properties": { "level": { "type": "string" }, "message": { "type": "string" }, "key": { "type": ["string", "null"] } } })),
                    "goals": live_message("Updated stream goals array.", "read_only, alerts", json!({ "type": "array" })),
                    "held": live_message("Message/event held by safety filters (`{ event_id, reason }`).", "read_only, chat", json!({ "type": "object" })),
                    "held_resolved": live_message("Held event released or dropped (`{ event_id, outcome }`).", "read_only, chat", json!({ "type": "object" })),
                    "panic": live_message("Panic lockdown toggled (`{ on, plan }`).", "read_only, chat", json!({ "type": "object" })),
                    "chatter": live_message("Updated tracked chatter metadata.", "read_only, chat", json!({ "type": "object" })),
                    "chat_highlight": live_message("Pinned or cleared featured chat message.", "read_only, chat", json!({ "type": "object" })),
                    "poll": live_message("Active poll progress or end state.", "read_only, chat", json!({ "type": "object" })),
                    "giveaway": live_message("Active giveaway state, entry count, or winner.", "read_only, chat", json!({ "type": "object" })),
                    "tts_pending": live_message("TTS queue items awaiting approval.", "read_only, chat, alerts", json!({ "type": "array" })),
                    "golive": live_message("Multistream output state and live bitrate telemetry.", "read_only, go_live, studio_control", json!({ "type": "object" })),
                    "smart_mod": live_message("Smart Moderation triage card.", "read_only, chat", json!({ "type": "object" })),
                    "update": live_message("Application update check/download state.", "read_only, studio_control", json!({ "type": "object" })),
                    "account": live_message("Ferrocast cloud account sign-in transition (PC session only, never sent to API keys).", "session only (blocked for API keys)", json!({ "type": "object" })),
                    "brb": live_message("OBS BRB scene state (`{ on, scene }`).", "read_only, studio_control, go_live", json!({ "type": "object" })),
                    "obs_health": live_message("OBS FPS/dropped-frames health snapshot.", "read_only, studio_control, go_live", json!({ "type": "object" })),
                    "live_alert": live_message("Live stream encoder/audio/network alert banner.", "read_only, studio_control, go_live", json!({ "type": "object" })),
                    "lane": live_message("Live Moments lane items.", "read_only, studio_control, go_live", json!({ "type": "object" })),
                    "marker": live_message("New stream marker created.", "read_only, studio_control, go_live", json!({ "type": "object" })),
                    "counter": live_message("Updated bot counter (`{ name, value }`).", "read_only, chat, studio_control", json!({ "type": "object" })),
                    "show": live_message("Show countdown and live rundown state (`{ show }`).", "read_only, studio_control, go_live", json!({ "type": "object" })),
                    "snapshot": live_message("Overlay reconnect full state snapshot.", "overlay channel only", json!({ "type": "object" }))
                }
            },
            "overlay": {
                "address": "/o/{token}/ws",
                "description": "Overlay browser-source WebSocket carrying widget snapshots, alerts, music visualizer frames, and display acks.",
                "parameters": {
                    "token": { "description": "Opaque overlay URL token." }
                },
                "messages": {
                    "snapshot": { "summary": "Initial full overlay config + widget state on connect.", "payload": { "type": "object" } },
                    "config": { "summary": "Updated overlay config.", "payload": { "type": "object" } },
                    "state": { "summary": "Updated widget/goal/poll/giveaway/credits state.", "payload": { "type": "object" } },
                    "event": { "summary": "Public chat/event card frame.", "payload": { "type": "object" } },
                    "alert": { "summary": "Show alert visual & sound frame.", "payload": { "type": "object" } },
                    "alert_hide": { "summary": "Dismiss active alert frame (`{ alert_id }`).", "payload": { "type": "object" } },
                    "tts": { "summary": "Play synthesized TTS audio in overlay.", "payload": { "type": "object" } },
                    "music": { "summary": "Visualizer spectrum levels & track info.", "payload": { "type": "object" } },
                    "delete_message": { "summary": "Remove moderated chat message from overlay chatbox.", "payload": { "type": "object" } },
                    "delete_user": { "summary": "Remove all messages from a timed-out/banned user.", "payload": { "type": "object" } },
                    "clear_chat": { "summary": "Clear overlay chatbox history.", "payload": { "type": "object" } },
                    "revoked": { "summary": "Overlay token was rotated or deleted; client should stop reconnecting.", "payload": { "type": "object" } },
                    "inbound_ack": { "summary": "Client -> server alert playback acknowledgment (`{ type: 'ack', alert_id, phase }`).", "payload": { "type": "object" } },
                    "inbound_visibility": { "summary": "Client -> server browser source visibility telemetry.", "payload": { "type": "object" } }
                }
            }
        },
        "operations": {
            "receiveLiveFrames": {
                "action": "receive",
                "channel": { "$ref": "#/channels/live" },
                "summary": "Subscribe to real-time filtered stream events and state updates over `/api/v1/live`."
            },
            "receiveOverlayFrames": {
                "action": "receive",
                "channel": { "$ref": "#/channels/overlay" },
                "summary": "Receive overlay alert and widget render frames on `/o/{token}/ws`."
            },
            "sendOverlayAck": {
                "action": "send",
                "channel": { "$ref": "#/channels/overlay" },
                "summary": "Report alert start/finish acknowledgement from overlay page."
            }
        }
    })
}
