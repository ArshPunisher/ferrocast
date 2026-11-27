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
    doc("POST", "/mod/ban", "Chat & Moderation", "Ban a chatter.", Some("ModBanRequest"), Some("BanResult")),
    doc("POST", "/mod/unban", "Chat & Moderation", "Unban a chatter.", Some("ModUnbanRequest"), None),
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
    doc(
        "GET",
        "/labels",
        "Alerts & Overlays",
        "Current evaluated stream label values by source key.",
        None,
        Some("LabelsMap"),
    ),
    doc("POST", "/media", "Designs & Media", "Upload local alert media asset (PC only).", None, None),
    doc("GET", "/media/{id}", "Designs & Media", "Serve a stored media asset by id.", None, None),
    doc("DELETE", "/media/{id}", "Designs & Media", "Delete an uploaded media asset.", None, None),
];

const CLIPS_ROUTES: &[RouteDoc] = &[
    doc(
        "GET",
        "/clips",
        "Clips",
        "The clips in the recordings folder, newest first (optional search by name; PC only).",
        None,
        None,
    ),
    doc(
        "POST",
        "/clips/compile",
        "Clips",
        "Join several clips into one in the background (crossfade, loudness; PC only).",
        None,
        None,
    ),
    doc(
        "GET",
        "/clips/jobs",
        "Clips",
        "Clip work running or finished (trim, compile, captions, burn, upload; PC only).",
        None,
        None,
    ),
    doc("GET", "/clips/jobs/{id}", "Clips", "One clip job's state and progress.", None, None),
    doc("DELETE", "/clips/jobs/{id}", "Clips", "Dismiss a finished clip job from the list.", None, None),
    doc("POST", "/clips/jobs/{id}/cancel", "Clips", "Stop a waiting or running clip job.", None, None),
    doc("GET", "/clips/{id}", "Clips", "What ffmpeg says about a clip: length, size, sound.", None, None),
    doc("PUT", "/clips/{id}", "Clips", "Rename a clip (its captions follow).", None, None),
    doc("DELETE", "/clips/{id}", "Clips", "Move a clip and its captions to the Trash.", None, None),
    doc("GET", "/clips/{id}/thumbnail", "Clips", "A small picture from the clip.", None, None),
    doc("GET", "/clips/{id}/file", "Clips", "The clip's bytes (byte ranges supported) for playback.", None, None),
    doc("POST", "/clips/{id}/trim", "Clips", "Cut part of a clip into a new clip (a job).", None, None),
    doc("GET", "/clips/{id}/captions", "Clips", "The clip's captions as cues.", None, None),
    doc("PUT", "/clips/{id}/captions", "Clips", "Save edited captions beside the clip.", None, None),
    doc("POST", "/clips/{id}/captions", "Clips", "Make captions with the local speech model (a job).", None, None),
    doc("GET", "/clips/{id}/captions.srt", "Clips", "The captions as a .srt download.", None, None),
    doc("POST", "/clips/{id}/captions/burn", "Clips", "Burn the captions into a new copy (a job).", None, None),
    doc("POST", "/clips/{id}/upload", "Clips", "Send the clip to YouTube (a job; private by default).", None, None),
];

const SCHEDULE_ROUTES: &[RouteDoc] = &[
    doc(
        "GET",
        "/schedule",
        "Go Live",
        "Planned streams, soonest first, with the scheduled broadcasts made for them.",
        None,
        Some("ScheduleList"),
    ),
    doc(
        "POST",
        "/schedule",
        "Go Live",
        "Plan a stream: title, category, platforms and start time. Makes scheduled broadcasts where a platform has them (YouTube, Facebook), reminds before the start, never goes live by itself. 201; 400 names the field, 409 over 50 plans.",
        Some("ScheduleCreate"),
        Some("ScheduledStream"),
    ),
    doc(
        "DELETE",
        "/schedule/{id}",
        "Go Live",
        "Remove a planned stream (broadcasts already made on a platform stay there).",
        None,
        None,
    ),
];

const GO_LIVE_ROUTES: &[RouteDoc] = &[
    doc(
        "GET",
        "/golive",
        "Go Live",
        "Multistream destinations, protection, encode groups/budget, and vertical canvas configuration.",
        None,
        Some("GoLiveView"),
    ),
    doc(
        "GET",
        "/golive/quality-options",
        "Go Live",
        "Per-destination allowed resolutions, fps, bitrates, soft-limit notes, encoder sharing, and PC encode budget.",
        None,
        Some("GoLiveQualityOptionsResponse"),
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
        "Add a custom RTMP/SRT destination with stream key (PC only). Optional brand (x, instagram, facebook) names a server and key pasted from that platform.",
        Some("GoLiveDestinationCreate"),
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
        "PUT",
        "/golive/destinations/{id}/quality",
        "Go Live",
        "Set per-destination stream quality (`same_as_main` or `custom`, PC only). While Studio is streaming, performs a single-destination live switch without interrupting other destinations.",
        Some("SetDestinationQualityRequest"),
        Some("GoLiveView"),
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
    doc(
        "GET",
        "/studio/engine/auto-config",
        "Studio Engine",
        "Encoder auto-config state: the running benchmark, its pick, and any unconfirmed apply.",
        None,
        Some("AutoConfigView"),
    ),
    doc(
        "POST",
        "/studio/engine/auto-config/benchmark",
        "Studio Engine",
        "Start the encoder benchmark (and optional upload check). Returns at once; progress arrives as `auto_config` frames.",
        Some("AutoConfigBenchmarkRequest"),
        Some("AutoConfigView"),
    ),
    doc(
        "POST",
        "/studio/engine/auto-config/cancel",
        "Studio Engine",
        "Cancel a running benchmark.",
        None,
        Some("AutoConfigView"),
    ),
    doc(
        "POST",
        "/studio/engine/auto-config/apply",
        "Studio Engine",
        "Apply the picked output settings (or explicit overrides) and start the 30 s confirm window and 1 min dropped-frame watch.",
        Some("AutoConfigApplyRequest"),
        Some("AutoConfigView"),
    ),
    doc(
        "POST",
        "/studio/engine/auto-config/keep",
        "Studio Engine",
        "Confirm the applied settings so they are kept.",
        None,
        Some("AutoConfigView"),
    ),
    doc(
        "POST",
        "/studio/engine/auto-config/undo",
        "Studio Engine",
        "Restore the output settings from before the apply.",
        None,
        Some("AutoConfigView"),
    ),
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
        "POST",
        "/studio/engine/scenes/{scene}/items/{id}/screenshot",
        "Studio Engine",
        "Save a PNG screenshot of a scene item with its filters and transform.",
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
        Some("EncodeBudgetResponse"),
    ),
    doc(
        "POST",
        "/studio/engine/encode-budget",
        "Studio Engine",
        "Estimate encode budget for proposed output settings.",
        Some("EncodeBudgetRequest"),
        Some("EncodeBudgetResponse"),
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
    doc("POST", "/studio/engine/transitions", "Studio Engine", "Add a named transition.", None, None),
    doc("PUT", "/studio/engine/transitions/{name}", "Studio Engine", "Update a named transition.", None, None),
    doc("DELETE", "/studio/engine/transitions/{name}", "Studio Engine", "Remove a named transition.", None, None),
    doc("POST", "/studio/engine/transitions/{name}/rename", "Studio Engine", "Rename a named transition.", None, None),
    doc(
        "POST",
        "/studio/engine/transitions/{name}/duplicate",
        "Studio Engine",
        "Duplicate a named transition.",
        None,
        None,
    ),
    doc(
        "PUT",
        "/studio/engine/transitions/{name}/default",
        "Studio Engine",
        "Make a named transition the default.",
        None,
        None,
    ),
    doc("PUT", "/studio/engine/transition-matrix", "Studio Engine", "Set per-scene transition choices.", None, None),
    doc(
        "PUT",
        "/studio/engine/scenes/{scene}/items/boxes",
        "Studio Engine",
        "Move and size several scene items at once.",
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
        "Save a PNG screenshot of a source, or a scene item when `scene` and `item` are given.",
        None,
        Some("StudioScreenshotResponse"),
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
        Some("StudioSourceStatus"),
    ),
    doc(
        "GET",
        "/studio/engine/screens/{id}/still",
        "Studio Engine",
        "Capture a static preview frame of a display.",
        None,
        Some("StudioSourceStill"),
    ),
    doc(
        "GET",
        "/studio/engine/still",
        "Studio Engine",
        "Fetch a static frame thumbnail.",
        None,
        Some("StudioSourceStill"),
    ),
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
        Some("StudioEngineStats"),
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
    doc(
        "GET",
        "/accounts",
        "Accounts & Pro",
        "Connected platform account statuses, logins, and public channel URLs (PC only).",
        None,
        None,
    ),
    doc(
        "GET",
        "/links",
        "Accounts & Pro",
        "Creator \"My links\" configuration and resolved overlay links/schedule values.",
        None,
        Some("LinksResponse"),
    ),
    doc(
        "PUT",
        "/links",
        "Accounts & Pro",
        "Partially update \"My links\" overrides, non-streaming socials, and custom links; omitted fields and unknown keys are preserved, `\"\"` or `null` clears string fields (`[]` or `null` clears `custom`), and live overlays update.",
        Some("CreatorLinks"),
        Some("LinksResponse"),
    ),
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

const STORE_ROUTES: &[RouteDoc] = &[
    doc(
        "GET",
        "/store/catalog",
        "Community Store",
        "The signed Store catalog with what is installed. `?fresh=true` fetches it again (PC only).",
        None,
        Some("StoreCatalogView"),
    ),
    doc(
        "GET",
        "/store/installed",
        "Community Store",
        "What the Store installed and the latest undo. Works when the catalog does not (PC only).",
        None,
        Some("StoreInstalledView"),
    ),
    doc(
        "GET",
        "/store/assets/{item_id}/{name}",
        "Community Store",
        "A picture or sound a catalog item lists, checked against its SHA-256 (PC only).",
        None,
        None,
    ),
    doc(
        "POST",
        "/store/install",
        "Community Store",
        "Install a catalog item, checking its package first. 201 with the undo (PC only, at this computer).",
        Some("StoreIdRequest"),
        Some("StoreMutationResult"),
    ),
    doc(
        "POST",
        "/store/update",
        "Community Store",
        "Update an installed item to the catalog's newer version (PC only, at this computer).",
        Some("StoreIdRequest"),
        Some("StoreMutationResult"),
    ),
    doc(
        "POST",
        "/store/uninstall",
        "Community Store",
        "Remove an installed item, giving back what it displaced (PC only, at this computer).",
        Some("StoreIdRequest"),
        Some("StoreMutationResult"),
    ),
    doc(
        "POST",
        "/store/undo",
        "Community Store",
        "Undo the latest Store action. Leave `token` out for the latest; one that is there must be the latest's own (PC only, at this computer).",
        Some("StoreUndoRequest"),
        Some("StoreUndoResult"),
    ),
    doc(
        "POST",
        "/store/resolve-link",
        "Community Store",
        "Read a `ferrocast://` Store link into a preview. It never installs: the person confirms first (PC only).",
        Some("StoreLinkRequest"),
        Some("StoreLinkPreview"),
    ),
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
        .chain(CLIPS_ROUTES)
        .chain(SCHEDULE_ROUTES)
        .chain(OBS_AND_MUSIC)
        .chain(STUDIO_ENGINE_ROUTES)
        .chain(MOMENTS_AND_SHOW)
        .chain(SETTINGS_AND_IMPORTS)
        .chain(ACCOUNTS_AND_PRO)
        .chain(PLUGINS_AND_DIAGNOSTICS)
        .chain(STORE_ROUTES)
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

const BINARY: fn() -> Value = || json!({ "type": "string", "format": "binary" });

/// The form an upload takes (the dashboard client sends `FormData`): `file`, or Streamlabs' fields.
fn multipart_form(method: &str, rel_path: &str) -> Option<Value> {
    match (method, rel_path) {
        ("POST", "/designs/import" | "/import" | "/media") => Some(json!({
            "type": "object",
            "required": ["file"],
            "properties": { "file": BINARY() }
        })),
        ("POST", "/import/streamlabs/preview") => Some(json!({
            "type": "object",
            "description": "Send the exports you have: each field is optional.",
            "properties": {
                "platform": { "type": "string", "enum": ["twitch", "youtube", "kick"], "description": "Whose viewers the points are." },
                "points": BINARY(),
                "tips": BINARY()
            }
        })),
        _ => None,
    }
}

/// A route that answers a file: its media type(s) and the `Content-Disposition` it names it with.
fn file_response(method: &str, rel_path: &str) -> Option<Value> {
    // A Store picture or sound is shown, not downloaded: no Content-Disposition, the type is the
    // item's own, and a locked-down CSP and `nosniff` come with it.
    if (method, rel_path) == ("GET", "/store/assets/{item_id}/{name}") {
        let types = ["image/png", "image/jpeg", "image/webp", "image/gif", "audio/mpeg", "audio/ogg", "audio/wav"];
        let content: Map<String, Value> =
            types.iter().map(|t| ((*t).to_string(), json!({ "schema": BINARY() }))).collect();
        return Some(json!({
            "description": "The asset's bytes (the type is the one the catalog lists).",
            "headers": {
                "Content-Security-Policy": { "description": "`default-src 'none'; sandbox`.", "schema": { "type": "string" } },
                "X-Content-Type-Options": { "description": "`nosniff`.", "schema": { "type": "string" } }
            },
            "content": content
        }));
    }
    let (types, what): (&[&str], &str) = match (method, rel_path) {
        ("POST", "/designs/{id}/export") => (&["application/zip"], "A `.ferrodesign` package (a zip)."),
        ("POST", "/export") => {
            (&["application/zip"], "A `.ferropack` backup bundle (a zip, streamed; it can be gigabytes).")
        }
        ("POST", "/diagnostics/report.zip") => (&["application/zip"], "The redacted support report (a zip)."),
        ("GET", "/pro/backups/key") => (&["text/plain"], "The cloud backup recovery key file (text)."),
        ("GET", "/moments/markers/export") => {
            (&["text/csv", "text/plain"], "The markers: CSV, or text (`format=csv|txt|youtube`).")
        }
        _ => return None,
    };
    let content: Map<String, Value> = types.iter().map(|t| ((*t).to_string(), json!({ "schema": BINARY() }))).collect();
    Some(json!({
        "description": what,
        "headers": {
            "Content-Disposition": {
                "description": "`attachment; filename=\"…\"`: the name to save it as.",
                "schema": { "type": "string" }
            }
        },
        "content": content
    }))
}

fn fresh_still_param() -> Value {
    json!({
        "name": "fresh",
        "in": "query",
        "required": false,
        "description": "Pass `1` (or `true`) to bypass the 15 s still cache and probe a fresh frame.",
        "schema": { "type": ["string", "integer", "boolean"] }
    })
}

fn still_width_param(default: u32) -> Value {
    json!({
        "name": "w",
        "in": "query",
        "required": false,
        "description": format!("Requested thumbnail width in pixels (default {default}, max 320)."),
        "schema": { "type": "integer", "default": default, "minimum": 1, "maximum": 320 }
    })
}

fn query_params(method: &str, rel_path: &str) -> Vec<Value> {
    match (method, rel_path) {
        ("GET", "/labels") => vec![json!({
            "name": "tz_offset_min",
            "in": "query",
            "required": false,
            "description": "Viewer timezone offset in minutes (`-720` to `840`, default `0` UTC) for monthly label calendar bounds.",
            "schema": { "type": "integer", "minimum": -720, "maximum": 840, "default": 0 }
        })],
        ("GET", "/store/catalog") => vec![json!({
            "name": "fresh",
            "in": "query",
            "required": false,
            "description": "`true` fetches and verifies the catalog again instead of using the one kept.",
            "schema": { "type": "boolean", "default": false }
        })],
        ("POST", "/studio/engine/sources/{name}/screenshot") => vec![
            json!({
                "name": "scene",
                "in": "query",
                "required": false,
                "description": "Scene name holding `item` when capturing a scene item with transforms and filters (must be paired with `item`).",
                "schema": { "type": "string" }
            }),
            json!({
                "name": "item",
                "in": "query",
                "required": false,
                "description": "Scene item id in `scene` to capture with transforms and filters (must be paired with `scene`).",
                "schema": { "type": "integer" }
            }),
        ],
        ("GET", "/studio/engine/screens/{id}/still") => vec![
            json!({
                "name": "window",
                "in": "query",
                "required": false,
                "description": "`true` treats `{id}` as a window id instead of a display id.",
                "schema": { "type": "boolean", "default": false }
            }),
            still_width_param(240),
            fresh_still_param(),
        ],
        ("GET", "/studio/engine/still") => vec![
            json!({
                "name": "kind",
                "in": "query",
                "required": true,
                "description": "Source input kind id (for example `av_capture_input_v2`).",
                "schema": { "type": "string" }
            }),
            json!({
                "name": "key",
                "in": "query",
                "required": true,
                "description": "Device setting key (for example `device`).",
                "schema": { "type": "string" }
            }),
            json!({
                "name": "value",
                "in": "query",
                "required": true,
                "description": "Selected device setting value.",
                "schema": { "type": "string" }
            }),
            still_width_param(240),
            fresh_still_param(),
        ],
        _ => Vec::new(),
    }
}

fn optional_req_schema(method: &str, rel_path: &str) -> Option<&'static str> {
    match (method, rel_path) {
        ("POST", "/studio/engine/sources/{name}/screenshot") => Some("StudioSourceScreenshotRequest"),
        _ => None,
    }
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

        let mut params = path_params(r.rel_path);
        params.extend(query_params(r.method, r.rel_path));
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
        } else if let Some(req_ref) = optional_req_schema(r.method, r.rel_path) {
            op.insert(
                "requestBody".into(),
                json!({
                    "required": false,
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

        // Uploads are forms, not JSON.
        if let Some(form) = multipart_form(r.method, r.rel_path) {
            op.insert(
                "requestBody".into(),
                json!({ "required": true, "content": { "multipart/form-data": { "schema": form } } }),
            );
        }

        let ok_status = match r.method {
            "POST" if r.rel_path == "/api-keys" => "201",
            "POST" if r.rel_path == "/store/install" => "201",
            "POST" if r.rel_path == "/schedule" => "201",
            "DELETE" => "204",
            _ => "200",
        };
        let ok_entry = if ok_status == "204" {
            json!({ "description": "Success (no content)" })
        } else if let Some(file) = file_response(r.method, r.rel_path) {
            file
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

        let mut responses = Map::new();
        responses.insert(ok_status.into(), ok_entry);
        if r.method == "PUT" && r.rel_path == "/golive/destinations/{id}/quality" {
            let err_schema = json!({ "$ref": "#/components/schemas/ApiError" });
            for (code, desc) in [
                (
                    "400",
                    "Invalid JSON, unknown field, invalid resolution/fps/bitrate, upscale above canvas, or VP9 codec.",
                ),
                ("404", "Unknown destination id."),
                ("409", "A live quality switch is already in progress for this destination."),
                ("415", "Missing or non-JSON Content-Type header."),
                (
                    "422",
                    "Over platform limit, custom quality locked (Direct/Twitch EB/Vertical Cut/host codec), exceeds PC encode budget, OBS streaming mode requires restart, or live keyframe handover failed/cancelled.",
                ),
            ] {
                responses.insert(
                    code.into(),
                    json!({ "description": desc, "content": { "application/json": { "schema": err_schema } } }),
                );
            }
        }
        responses.insert("401".into(), json!({ "description": "Missing, invalid, revoked, or expired credentials.", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/ApiError" } } } }));
        responses.insert("403".into(), json!({ "description": "Insufficient API key scope, forbidden origin, or PC-only endpoint.", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/ApiError" } } } }));
        responses.insert("429".into(), json!({ "description": "Per-key rate limit exceeded (see Retry-After header).", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/ApiError" } } } }));
        op.insert("responses".into(), Value::Object(responses));

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

/// The Community Store schemas, one `json!` each (a single one this size passes the macro
/// recursion limit).
fn store_schemas() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("StoreIdRequest".into(), json!({
        "type": "object",
        "required": ["id"],
        "properties": { "id": { "type": "string", "description": "A catalog item id (3 to 80 of a-z, 0-9, `.`, `-`, `_`)." } }
    }));
    m.insert("StoreUndoRequest".into(), json!({
        "type": "object",
        "description": "Leave `token` out to undo the latest action. A `token` that is there (even null or empty) must be the latest's own: 400 `store_undo_bad_token` if it is not text, 404 `store_undo_unknown`, 409 `store_undo_out_of_order`; 409 `store_undo_conflict` when the user's own default look was edited since a removal put it back.",
        "properties": { "token": { "type": "string", "minLength": 1 } }
    }));
    m.insert("StoreLinkRequest".into(), json!({
        "type": "object",
        "required": ["uri"],
        "properties": { "uri": { "type": "string", "maxLength": 512, "description": "`ferrocast://store/install/<id>`, `ferrocast://install/<id>` or `ferrocast://store/item/<id>`, with an optional `?v=<version>`." } }
    }));
    m.insert(
        "StoreUndoSummary".into(),
        json!({
            "type": "object",
            "required": ["token", "action", "item_id", "item_title", "created_at"],
            "properties": {
                "token": { "type": "string" },
                "action": { "type": "string", "enum": ["install", "update", "uninstall"] },
                "item_id": { "type": "string" },
                "item_title": { "type": "string" },
                "created_at": { "type": "integer", "description": "Unix milliseconds." }
            }
        }),
    );
    m.insert(
        "StoreCreator".into(),
        json!({
            "type": "object",
            "required": ["id", "name", "handle"],
            "properties": {
                "id": { "type": "string" },
                "name": { "type": "string" },
                "handle": { "type": "string" },
                "bio": { "type": "string" },
                "verified": { "type": "boolean" },
                "links": {
                    "type": "object",
                    "properties": {
                        "website": { "type": ["string", "null"] },
                        "twitch": { "type": ["string", "null"] },
                        "youtube": { "type": ["string", "null"] },
                        "github": { "type": ["string", "null"] }
                    }
                }
            }
        }),
    );
    let package = json!({
        "type": "object",
        "required": ["format", "url", "sha256", "size"],
        "properties": {
            "format": { "type": "string", "enum": ["ferrodesign", "ferroalert", "ferroplugin"] },
            "url": { "type": "string" },
            "sha256": { "type": "string" },
            "size": { "type": "integer" }
        }
    });
    let assets = json!({
        "type": "array",
        "items": {
            "type": "object",
            "required": ["name", "role", "url", "sha256", "size", "mime"],
            "properties": {
                "name": { "type": "string" },
                "role": { "type": "string" },
                "url": { "type": "string" },
                "sha256": { "type": "string" },
                "size": { "type": "integer" },
                "mime": { "type": "string" }
            }
        }
    });
    let stats = json!({
        "type": "object",
        "required": ["installs", "rating_avg", "rating_count"],
        "properties": {
            "installs": { "type": "integer" },
            "rating_avg": { "type": "number" },
            "rating_count": { "type": "integer" }
        }
    });
    let custom_alert = json!({
        "type": ["object", "null"],
        "properties": {
            "html": { "type": "string" },
            "css": { "type": "string" },
            "fields": { "type": "array", "items": { "type": "object" } },
            "field_values": { "type": "object", "additionalProperties": true }
        }
    });
    let preview = json!({
        "type": "object",
        "required": ["palette", "style", "motion"],
        "properties": {
            "palette": { "type": "array", "minItems": 3, "maxItems": 3, "items": { "type": "string" } },
            "style": { "type": "string", "enum": ["minimal", "bold", "retro", "playful", "tech", "cozy"] },
            "motion": { "type": "string", "enum": ["still", "smooth", "lively", "bouncy"] },
            "headline": { "type": ["string", "null"] },
            "subline": { "type": ["string", "null"] },
            "layers_hint": { "type": ["integer", "null"] },
            "design": { "type": ["object", "null"], "description": "A design, as `GET /designs/{id}` returns one." },
            "custom_alert": custom_alert
        }
    });
    let events = json!({
        "type": "array",
        "items": { "type": "string", "enum": ["follow", "sub", "resub", "gift", "member", "bits", "kicks", "tip", "superchat", "raid", "redeem"] }
    });
    m.insert("StoreItem".into(), json!({
        "type": "object",
        "required": ["id", "kind", "title", "summary", "version", "creator_id", "license", "package", "assets", "stats", "preview", "installed_version", "update_available"],
        "properties": {
            "id": { "type": "string" },
            "kind": { "type": "string", "enum": ["theme", "alert", "widget", "overlay", "plugin"] },
            "title": { "type": "string" },
            "summary": { "type": "string" },
            "version": { "type": "string" },
            "creator_id": { "type": "string" },
            "tags": { "type": "array", "items": { "type": "string" } },
            "license": { "type": "string" },
            "sandboxed": { "type": "boolean" },
            "plugin_kind": { "type": ["string", "null"] },
            "plugin_actions": { "type": "array", "items": { "type": "string" } },
            "alert_events": events,
            "featured": { "type": "boolean" },
            "created_at": { "type": "integer" },
            "updated_at": { "type": "integer" },
            "package": package,
            "assets": assets,
            "stats": stats,
            "preview": preview,
            "installed_version": { "type": ["string", "null"] },
            "update_available": { "type": "boolean" }
        }
    }));
    m.insert("StoreInstalledItem".into(), json!({
        "type": "object",
        "required": ["item_id", "kind", "format", "title", "version", "creator_id", "creator_name", "installed_at", "updated_at", "package_sha256", "design_ids", "rule_ids", "plugin_id", "can_undo", "undo_token", "latest_version", "update_available"],
        "properties": {
            "item_id": { "type": "string" },
            "kind": { "type": "string", "enum": ["theme", "alert", "widget", "overlay", "plugin"] },
            "format": { "type": "string", "enum": ["ferrodesign", "ferroalert", "ferroplugin"] },
            "title": { "type": "string" },
            "version": { "type": "string" },
            "creator_id": { "type": "string" },
            "creator_name": { "type": "string" },
            "installed_at": { "type": "integer" },
            "updated_at": { "type": "integer" },
            "package_sha256": { "type": "string" },
            "design_ids": { "type": "array", "items": { "type": "string" } },
            "rule_ids": { "type": "array", "items": { "type": "string" } },
            "plugin_id": { "type": ["string", "null"] },
            "can_undo": { "type": "boolean", "description": "This item's action is the latest one, so it can be undone." },
            "undo_token": { "type": ["string", "null"] },
            "latest_version": { "type": ["string", "null"] },
            "update_available": { "type": "boolean" }
        }
    }));
    m.insert("StoreCatalogView".into(), json!({
        "type": "object",
        "required": ["version", "generated_at", "fetched_at", "stale", "creators", "items", "installed", "last_undo", "warnings"],
        "properties": {
            "version": { "type": "integer" },
            "generated_at": { "type": "integer" },
            "fetched_at": { "type": "integer" },
            "stale": { "type": "boolean" },
            "creators": { "type": "array", "items": { "$ref": "#/components/schemas/StoreCreator" } },
            "items": { "type": "array", "items": { "$ref": "#/components/schemas/StoreItem" } },
            "installed": { "type": "array", "items": { "$ref": "#/components/schemas/StoreInstalledItem" } },
            "last_undo": { "oneOf": [{ "$ref": "#/components/schemas/StoreUndoSummary" }, { "type": "null" }] },
            "warnings": { "type": "array", "items": { "type": "string" } }
        }
    }));
    m.insert(
        "StoreInstalledView".into(),
        json!({
            "type": "object",
            "required": ["installed", "last_undo"],
            "properties": {
                "installed": { "type": "array", "items": { "$ref": "#/components/schemas/StoreInstalledItem" } },
                "last_undo": { "oneOf": [{ "$ref": "#/components/schemas/StoreUndoSummary" }, { "type": "null" }] }
            }
        }),
    );
    m.insert("StoreMutationResult".into(), json!({
        "type": "object",
        "required": ["action", "installed", "designs", "undo"],
        "properties": {
            "action": { "type": "string", "enum": ["install", "update", "uninstall"] },
            "installed": { "oneOf": [{ "$ref": "#/components/schemas/StoreInstalledItem" }, { "type": "null" }] },
            "designs": { "type": "array", "items": { "type": "object" }, "description": "The designs the action added (designs, as `GET /designs/{id}` returns them)." },
            "undo": { "$ref": "#/components/schemas/StoreUndoSummary" }
        }
    }));
    m.insert(
        "StoreUndoResult".into(),
        json!({
            "type": "object",
            "required": ["reverted_action", "item_id", "item_title", "installed"],
            "properties": {
                "reverted_action": { "type": "string", "enum": ["install", "update", "uninstall"] },
                "item_id": { "type": "string" },
                "item_title": { "type": "string" },
                "installed": { "type": "array", "items": { "$ref": "#/components/schemas/StoreInstalledItem" } }
            }
        }),
    );
    m.insert("StoreLinkPreview".into(), json!({
        "type": "object",
        "required": ["uri", "action", "item", "creator", "installed", "update_available", "safety_notes", "requires_confirmation"],
        "properties": {
            "uri": { "type": "string" },
            "action": { "type": "string", "enum": ["install", "view"] },
            "item": { "$ref": "#/components/schemas/StoreItem" },
            "creator": { "$ref": "#/components/schemas/StoreCreator" },
            "installed": { "oneOf": [{ "$ref": "#/components/schemas/StoreInstalledItem" }, { "type": "null" }] },
            "update_available": { "type": "boolean" },
            "safety_notes": { "type": "array", "items": { "type": "string" }, "description": "What is already true, and what will be checked when the person installs." },
            "requires_confirmation": { "type": "boolean", "const": true }
        }
    }));
    m
}

/// The stream scheduler's and Go live's destination schemas.
fn schedule_schemas() -> Map<String, Value> {
    let mut m = Map::new();
    let platforms = json!({ "type": "array", "minItems": 1, "items": { "type": "string", "enum": ["twitch", "youtube", "kick", "facebook"] } });
    m.insert("ScheduleCreate".into(), json!({
        "type": "object",
        "required": ["info", "platforms", "start_ms"],
        "properties": {
            "info": {
                "type": "object",
                "description": "What the stream is about. `title` is required (at most 140 characters, no control characters).",
                "required": ["title"],
                "properties": {
                    "title": { "type": "string", "minLength": 1, "maxLength": 140 },
                    "category": {
                        "type": ["object", "null"],
                        "description": "`{ name, ids? }`: the category's name and, where known, each platform's id.",
                        "required": ["name"],
                        "properties": { "name": { "type": "string", "minLength": 1, "maxLength": 100 }, "ids": { "type": "object" } }
                    },
                    "description": { "type": ["string", "null"] },
                    "tags": { "type": ["array", "null"], "items": { "type": "string" } },
                    "language": { "type": ["string", "null"] }
                }
            },
            "platforms": platforms.clone(),
            "start_ms": { "type": "integer", "description": "Unix milliseconds: in the future, at most a year ahead." },
            "remind_minutes": { "type": "integer", "minimum": 1, "maximum": 1440, "default": 10 }
        }
    }));
    m.insert("ScheduledStream".into(), json!({
        "type": "object",
        "required": ["id", "info", "platforms", "start_ms", "remind_minutes", "reminded", "broadcasts"],
        "properties": {
            "id": { "type": "string" },
            "info": { "type": "object" },
            "platforms": platforms,
            "start_ms": { "type": "integer" },
            "remind_minutes": { "type": "integer" },
            "reminded": { "type": "boolean" },
            "broadcasts": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["platform"],
                    "properties": {
                        "platform": { "type": "string" },
                        "id": { "type": "string", "description": "The platform's id for the scheduled broadcast, when one was made." },
                        "url": { "type": "string", "description": "An https address of it, when the platform gave one." },
                        "note": { "type": "string", "description": "Why there is no broadcast: not connected, none to make there, or the platform said no." }
                    }
                }
            }
        }
    }));
    m.insert("ScheduleList".into(), json!({
        "type": "object",
        "required": ["streams"],
        "properties": { "streams": { "type": "array", "items": { "$ref": "#/components/schemas/ScheduledStream" } } }
    }));
    m.insert("GoLiveDestinationCreate".into(), json!({
        "type": "object",
        "required": ["name", "url"],
        "properties": {
            "name": { "type": "string", "minLength": 1, "maxLength": 40 },
            "url": { "type": "string", "description": "rtmp://, rtmps://, srt://, rist:// or https:// (WebRTC WHIP)." },
            "key": { "type": "string", "maxLength": 512, "description": "No spaces. Never returned. Needed for RTMP and RTMPS; optional for SRT (stream id), RIST (passphrase) and WebRTC (bearer token)." },
            "canvas": { "type": "string", "enum": ["main", "vertical"] },
            "brand": { "type": "string", "enum": ["x", "instagram", "facebook"], "description": "Names a server and key pasted from that platform and applies that platform's stream limits." }
        }
    }));
    m
}

fn golive_quality_schemas() -> Map<String, Value> {
    let mut m = Map::new();
    let codec_enum = json!({ "type": "string", "enum": ["h264", "hevc", "av1", "vp9"] });
    let quality_one_of = json!({
        "oneOf": [
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["mode"],
                "properties": {
                    "mode": { "const": "same_as_main" }
                }
            },
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["mode", "width", "height", "fps", "kbps"],
                "properties": {
                    "mode": { "const": "custom" },
                    "width": { "type": "integer", "minimum": 2 },
                    "height": { "type": "integer", "minimum": 2 },
                    "fps": { "type": "integer", "minimum": 1 },
                    "kbps": { "type": "integer", "minimum": 500, "maximum": 60000 },
                    "codec": { "oneOf": [codec_enum.clone(), { "type": "null" }] }
                }
            }
        ]
    });
    m.insert("DestinationQuality".into(), quality_one_of.clone());
    m.insert("SetDestinationQualityRequest".into(), quality_one_of);
    m.insert(
        "ResolvedQuality".into(),
        json!({
            "type": "object",
            "required": [
                "width", "height", "fps", "kbps", "codec", "audio_kbps",
                "auto_fitted", "separate_encode", "group_id", "shared_with"
            ],
            "properties": {
                "width": { "type": "integer" },
                "height": { "type": "integer" },
                "fps": { "type": "integer" },
                "kbps": { "type": "integer" },
                "codec": codec_enum.clone(),
                "audio_kbps": { "type": "integer" },
                "auto_fitted": { "type": "boolean" },
                "separate_encode": { "type": "boolean" },
                "group_id": { "type": "string" },
                "shared_with": { "type": "array", "items": { "type": "string" } }
            }
        }),
    );
    m.insert(
        "EncodeGroupView".into(),
        json!({
            "type": "object",
            "required": [
                "id", "canvas", "width", "height", "fps", "kbps", "codec", "members",
                "live_fps", "live_kbps", "dropped_frames", "encode_ms"
            ],
            "properties": {
                "id": { "type": "string" },
                "canvas": { "type": "string", "enum": ["main", "vertical"] },
                "width": { "type": "integer" },
                "height": { "type": "integer" },
                "fps": { "type": "integer" },
                "kbps": { "type": "integer" },
                "codec": codec_enum.clone(),
                "members": { "type": "array", "items": { "type": "string" } },
                "live_fps": { "type": ["number", "null"] },
                "live_kbps": { "type": ["integer", "null"] },
                "dropped_frames": { "type": ["integer", "null"] },
                "encode_ms": { "type": ["number", "null"] }
            }
        }),
    );
    m.insert(
        "EncodeBudgetCheck".into(),
        json!({
            "type": "object",
            "required": ["status", "source", "used_mpx_s", "limit_mpx_s", "used_sessions", "max_sessions"],
            "properties": {
                "status": { "type": "string", "enum": ["ok", "limit", "over", "unknown"] },
                "source": { "type": "string", "enum": ["apple_silicon", "bench", "nvenc_default", "hw_default", "cpu_default", "unknown"] },
                "used_mpx_s": { "type": "number" },
                "limit_mpx_s": { "type": ["number", "null"] },
                "used_sessions": { "type": "integer" },
                "max_sessions": { "type": ["integer", "null"] },
                "note": { "type": ["string", "null"] }
            }
        }),
    );
    m.insert(
        "DestinationView".into(),
        json!({
            "type": "object",
            "required": [
                "id", "kind", "canvas", "name", "account", "enabled", "connected",
                "health", "blocked", "rules", "issues", "quality", "resolved_quality", "switching_quality"
            ],
            "properties": {
                "id": { "type": "string" },
                "kind": { "type": "string", "enum": ["twitch", "youtube", "kick", "custom"] },
                "canvas": { "type": "string", "enum": ["main", "vertical"] },
                "name": { "type": "string" },
                "account": { "type": ["string", "null"] },
                "enabled": { "type": "boolean" },
                "connected": { "type": "boolean" },
                "health": { "type": ["object", "null"], "additionalProperties": true },
                "blocked": { "type": ["string", "null"] },
                "brand": { "type": ["string", "null"] },
                "rules": { "type": "array", "items": { "type": "string" } },
                "issues": { "type": "array", "items": { "type": "object", "additionalProperties": true } },
                "capped": { "type": ["object", "null"], "additionalProperties": true },
                "direct": { "type": ["string", "null"], "enum": ["srt", "rist", "whip", null] },
                "light": { "type": "boolean" },
                "enhanced": { "type": "boolean" },
                "lighter_for_route": { "type": "boolean" },
                "backup_for": { "type": ["string", "null"] },
                "standby_for": { "type": ["string", "null"] },
                "key_note": { "type": ["string", "null"] },
                "quality_note": { "type": ["string", "null"] },
                "quality": { "$ref": "#/components/schemas/DestinationQuality" },
                "resolved_quality": { "$ref": "#/components/schemas/ResolvedQuality" },
                "switching_quality": { "type": "boolean" }
            }
        }),
    );
    m.insert(
        "GoLiveView".into(),
        json!({
            "type": "object",
            "required": [
                "ingest", "obs", "live", "stats", "vertical_live", "vertical_stats",
                "destinations", "encode_groups", "encode_budget", "upload",
                "protect", "aitum_ok", "cut", "advanced", "auto_quality"
            ],
            "properties": {
                "ingest": { "type": ["string", "null"] },
                "obs": { "type": "object", "additionalProperties": true },
                "live": { "type": "boolean" },
                "stats": { "type": "object", "additionalProperties": true },
                "vertical_live": { "type": "boolean" },
                "vertical_stats": { "type": "object", "additionalProperties": true },
                "destinations": { "type": "array", "items": { "$ref": "#/components/schemas/DestinationView" } },
                "encode_groups": { "type": "array", "items": { "$ref": "#/components/schemas/EncodeGroupView" } },
                "encode_budget": { "$ref": "#/components/schemas/EncodeBudgetCheck" },
                "upload": { "type": ["object", "null"], "additionalProperties": true },
                "protect": { "type": "boolean" },
                "aitum_ok": { "type": "boolean" },
                "cut": { "type": "object", "additionalProperties": true },
                "advanced": { "type": "object", "additionalProperties": true },
                "auto_quality": { "type": "boolean" }
            }
        }),
    );
    m.insert(
        "QualityCanvasSummary".into(),
        json!({
            "type": "object",
            "required": ["width", "height", "fps", "kbps", "codec", "audio_kbps"],
            "properties": {
                "width": { "type": "integer" },
                "height": { "type": "integer" },
                "fps": { "type": "integer" },
                "kbps": { "type": "integer" },
                "codec": codec_enum.clone(),
                "audio_kbps": { "type": "integer" }
            }
        }),
    );
    m.insert(
        "QualityOptionResolved".into(),
        json!({
            "type": "object",
            "required": ["width", "height", "fps", "kbps", "codec"],
            "properties": {
                "width": { "type": "integer" },
                "height": { "type": "integer" },
                "fps": { "type": "integer" },
                "kbps": { "type": "integer" },
                "codec": codec_enum.clone()
            }
        }),
    );
    m.insert(
        "QualityOption".into(),
        json!({
            "type": "object",
            "required": [
                "id", "label", "quality", "resolved", "auto_fitted", "recommended",
                "note", "shares_encoder_with", "allowed", "disabled_reason"
            ],
            "properties": {
                "id": { "type": "string" },
                "label": { "type": "string" },
                "quality": { "$ref": "#/components/schemas/DestinationQuality" },
                "resolved": { "$ref": "#/components/schemas/QualityOptionResolved" },
                "auto_fitted": { "type": "boolean" },
                "recommended": { "type": "boolean" },
                "note": { "type": ["string", "null"] },
                "shares_encoder_with": { "type": "array", "items": { "type": "string" } },
                "allowed": { "type": "boolean" },
                "disabled_reason": { "type": ["string", "null"] }
            }
        }),
    );
    m.insert(
        "CustomQualityLimits".into(),
        json!({
            "type": "object",
            "required": [
                "max_width", "max_height", "min_kbps", "max_kbps", "fps_choices", "codecs",
                "recommended_width", "recommended_height", "recommended_fps", "recommended_kbps"
            ],
            "properties": {
                "max_width": { "type": "integer" },
                "max_height": { "type": "integer" },
                "min_kbps": { "type": "integer" },
                "max_kbps": { "type": "integer" },
                "fps_choices": { "type": "array", "items": { "type": "integer" } },
                "codecs": { "type": "array", "items": codec_enum },
                "recommended_width": { "type": "integer" },
                "recommended_height": { "type": "integer" },
                "recommended_fps": { "type": "integer" },
                "recommended_kbps": { "type": "integer" }
            }
        }),
    );
    m.insert(
        "DestinationQualityOptions".into(),
        json!({
            "type": "object",
            "required": ["id", "canvas", "locked_reason", "current", "resolved", "options", "custom_limits"],
            "properties": {
                "id": { "type": "string" },
                "canvas": { "type": "string", "enum": ["main", "vertical"] },
                "locked_reason": { "type": ["string", "null"] },
                "current": { "$ref": "#/components/schemas/DestinationQuality" },
                "resolved": { "$ref": "#/components/schemas/ResolvedQuality" },
                "options": { "type": "array", "items": { "$ref": "#/components/schemas/QualityOption" } },
                "custom_limits": { "oneOf": [{ "$ref": "#/components/schemas/CustomQualityLimits" }, { "type": "null" }] }
            }
        }),
    );
    m.insert(
        "GoLiveQualityOptionsResponse".into(),
        json!({
            "type": "object",
            "required": ["source", "main", "vertical", "budget", "destinations"],
            "properties": {
                "source": { "type": "string", "enum": ["studio", "obs", "none"] },
                "main": { "$ref": "#/components/schemas/QualityCanvasSummary" },
                "vertical": { "oneOf": [{ "$ref": "#/components/schemas/QualityCanvasSummary" }, { "type": "null" }] },
                "budget": { "$ref": "#/components/schemas/EncodeBudgetCheck" },
                "destinations": {
                    "type": "object",
                    "additionalProperties": { "$ref": "#/components/schemas/DestinationQualityOptions" }
                }
            }
        }),
    );
    m.insert(
        "EncodeBudgetRequest".into(),
        json!({
            "type": "object",
            "required": ["settings"],
            "properties": {
                "settings": { "type": "object", "additionalProperties": true }
            }
        }),
    );
    m.insert(
        "EncodeBudgetResponse".into(),
        json!({
            "type": "object",
            "required": [
                "replay", "budget_mpx", "stream_mpx", "record_mpx", "shared", "why_not_shared", "level"
            ],
            "properties": {
                "replay": { "type": "boolean" },
                "budget_mpx": { "type": ["number", "null"] },
                "stream_mpx": { "type": "number" },
                "record_mpx": { "type": "number" },
                "shared": { "type": "boolean" },
                "why_not_shared": { "type": ["string", "null"] },
                "level": { "type": "string", "enum": ["ok", "limit", "over", "unknown"] },
                "vertical_mpx": { "type": "number" },
                "used_mpx_s": { "type": "number" },
                "limit_mpx_s": { "type": ["number", "null"] },
                "used_sessions": { "type": "integer" },
                "max_sessions": { "type": ["integer", "null"] },
                "budget_source": {
                    "type": "string",
                    "enum": ["apple_silicon", "bench", "nvenc_default", "hw_default", "cpu_default", "unknown"]
                },
                "budget_status": { "type": "string", "enum": ["ok", "limit", "over", "unknown"] },
                "budget_note": { "type": ["string", "null"] },
                "group_count": { "type": "integer" }
            }
        }),
    );
    m
}

/// The encoder auto-config schemas. Built one by one: a single `json!` this size passes the macro
/// recursion limit.
fn auto_config_schemas() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("AutoConfigBenchmarkRequest".into(), json!({
            "type": "object",
            "properties": {
                "mode": { "type": "string", "enum": ["stream", "record"], "description": "Default `stream`." },
                "fps_preference": { "type": "integer", "enum": [30, 60], "description": "Default 60." },
                "include_upload": { "type": "boolean", "description": "Measure upload first. Default true for `stream`, false for `record`." },
                "max_seconds": { "type": "integer", "minimum": 2, "maximum": 35, "description": "Time budget for the encoder tests. Default 24." }
            }
        }));
    m.insert("AutoConfigApplyRequest".into(), json!({
            "type": "object",
            "description": "Every field is optional: what is left out comes from the benchmark's pick. `video_encoder` and `scale` accept an explicit null (clear it).",
            "properties": {
                "encoder": { "type": "string", "enum": ["hardware", "software"] },
                "video_encoder": { "type": ["string", "null"] },
                "encoder_settings": { "type": "object", "additionalProperties": true },
                "scale": {
                    "type": ["array", "null"],
                    "minItems": 2,
                    "maxItems": 2,
                    "items": { "type": "integer", "minimum": 1 }
                },
                "fps": { "type": "integer", "enum": [24, 25, 30, 48, 50, 60, 120] },
                "video_kbps": { "type": "integer", "minimum": 500, "maximum": 60000 },
                "label": { "type": "string" }
            }
        }));
    m.insert(
        "AutoConfigAppliedSummary".into(),
        json!({
            "type": "object",
            "required": ["encoder", "video_encoder", "encoder_settings", "scale", "fps", "video_kbps", "label"],
            "properties": {
                "encoder": { "type": "string" },
                "video_encoder": { "type": ["string", "null"] },
                "encoder_settings": { "type": "object", "additionalProperties": true },
                "scale": {
                    "type": ["array", "null"],
                    "minItems": 2,
                    "maxItems": 2,
                    "items": { "type": "integer" }
                },
                "fps": { "type": "integer" },
                "video_kbps": { "type": "integer" },
                "label": { "type": "string" }
            }
        }),
    );
    m.insert("AutoConfigCandidate".into(), json!({
            "type": "object",
            "description": "One encoder setting that was tested, with its verdict in plain words.",
            "required": ["label", "family", "hardware", "width", "height", "fps", "bitrate_kbps", "passed", "reason"],
            "properties": {
                "encoder_backend": { "type": "string" },
                "studio_encoder_id": { "type": "string" },
                "label": { "type": "string" },
                "family": { "type": "string", "enum": ["video_toolbox", "nvenc", "qsv", "amf", "media_foundation", "x264"] },
                "hardware": { "type": "boolean" },
                "preset": { "type": ["string", "null"] },
                "width": { "type": "integer" },
                "height": { "type": "integer" },
                "fps": { "type": "integer" },
                "bitrate_kbps": { "type": "integer" },
                "frames_tested": { "type": "integer" },
                "encode_ms_avg": { "type": "number" },
                "encode_ms_p95": { "type": "number" },
                "encode_ms_max": { "type": "number" },
                "budget_ms": { "type": "number" },
                "fps_achieved": { "type": "number" },
                "headroom_pct": { "type": "number" },
                "skipped_frames": { "type": "integer" },
                "lagged_frames": { "type": "integer" },
                "cpu_pct": { "type": ["number", "null"] },
                "gpu_pct": { "type": ["number", "null"] },
                "elapsed_ms": { "type": "integer" },
                "passed": { "type": "boolean" },
                "reason": { "type": "string" }
            }
        }));
    m.insert("AutoConfigRecommendation".into(), json!({
            "type": "object",
            "description": "The benchmark's pick.",
            "required": ["encoder", "video_encoder", "encoder_settings", "label", "fps", "video_kbps", "limited_by", "why"],
            "properties": {
                "encoder": { "type": "string" },
                "video_encoder": { "type": ["string", "null"] },
                "studio_encoder_id": { "type": "string" },
                "encoder_settings": { "type": "object", "additionalProperties": true },
                "label": { "type": "string" },
                "preset": { "type": ["string", "null"] },
                "scale": {
                    "type": ["array", "null"],
                    "minItems": 2,
                    "maxItems": 2,
                    "items": { "type": "integer" }
                },
                "width": { "type": "integer" },
                "height": { "type": "integer" },
                "fps": { "type": "integer" },
                "video_kbps": { "type": "integer" },
                "headroom_pct": { "type": "number" },
                "limited_by": { "type": "string", "enum": ["headroom", "upload", "platform_cap", "balanced"] },
                "why": { "type": "string" }
            }
        }));
    m.insert("AutoConfigUploadCheck".into(), json!({
            "type": "object",
            "description": "The upload speed check that went before the benchmark.",
            "required": ["at", "measured_by", "rows", "verdict"],
            "properties": {
                "at": { "type": "integer", "description": "Unix milliseconds." },
                "measured_by": { "type": "string", "description": "`twitch` (bandwidth-test mode) or `estimate`." },
                "rows": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["id", "name", "need_kbps"],
                        "properties": {
                            "id": { "type": "string" },
                            "name": { "type": "string" },
                            "server": { "type": ["string", "null"] },
                            "delay_ms": { "type": ["integer", "null"] },
                            "need_kbps": { "type": "integer" },
                            "problem": { "type": "string", "description": "Present only when the platform could not be checked." }
                        }
                    }
                },
                "verdict": {
                    "type": "object",
                    "required": ["upload_kbps", "need_kbps", "fits", "fit_count", "message"],
                    "properties": {
                        "upload_kbps": { "type": "integer" },
                        "need_kbps": { "type": "integer" },
                        "fits": { "type": "boolean" },
                        "fit_count": { "type": "integer" },
                        "message": { "type": "string" }
                    }
                }
            }
        }));
    m.insert("AutoConfigView".into(), json!({
            "type": "object",
            "description": "The auto-config state machine, as the REST routes answer and as the `auto_config` live frame carries.",
            "required": [
                "state", "job_id", "mode", "phase", "progress_pct", "current_label", "elapsed_ms", "estimated_total_ms",
                "candidates", "upload_check", "upload_error", "picked", "duration_ms", "error", "applied", "confirmed",
                "confirm_remaining_s", "watch_remaining_s", "first_minute_drops", "first_minute_lags", "rollback_offered",
                "rollback_trigger", "rollback_reason", "restored"
            ],
            "properties": {
                "state": { "type": "string", "enum": ["idle", "running", "failed", "done", "applied", "rolled_back"] },
                "job_id": { "type": ["string", "null"] },
                "mode": { "type": "string", "enum": ["stream", "record"] },
                "phase": { "type": ["string", "null"], "enum": ["upload", "discovering", "encoding", "picking", null] },
                "progress_pct": { "type": "integer", "minimum": 0, "maximum": 100 },
                "current_label": { "type": ["string", "null"] },
                "elapsed_ms": { "type": "integer" },
                "estimated_total_ms": { "type": "integer" },
                "candidates": { "type": "array", "items": { "$ref": "#/components/schemas/AutoConfigCandidate" } },
                "upload_check": { "oneOf": [{ "$ref": "#/components/schemas/AutoConfigUploadCheck" }, { "type": "null" }] },
                "upload_error": { "type": ["string", "null"] },
                "picked": { "oneOf": [{ "$ref": "#/components/schemas/AutoConfigRecommendation" }, { "type": "null" }] },
                "duration_ms": { "type": ["integer", "null"] },
                "error": { "type": ["string", "null"] },
                "applied": { "oneOf": [{ "$ref": "#/components/schemas/AutoConfigAppliedSummary" }, { "type": "null" }] },
                "confirmed": { "type": "boolean" },
                "confirm_remaining_s": { "type": "integer", "description": "Seconds left to keep the applied settings before they roll back." },
                "watch_remaining_s": { "type": "integer", "description": "Seconds left of the first-minute dropped-frame watch." },
                "first_minute_drops": { "type": "integer" },
                "first_minute_lags": { "type": "integer" },
                "rollback_offered": { "type": "boolean" },
                "rollback_trigger": { "type": ["string", "null"], "enum": ["user_undo", "timeout", "dropped_frames", null] },
                "rollback_reason": { "type": ["string", "null"] },
                "restored": { "oneOf": [{ "$ref": "#/components/schemas/AutoConfigAppliedSummary" }, { "type": "null" }] }
            }
        }));
    m
}

fn schemas() -> Value {
    let mut doc = json!({
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
            "required": ["platform", "user_id", "seconds"],
            "properties": {
                "platform": { "type": "string" },
                "user_id": { "type": "string" },
                "seconds": { "type": "integer", "minimum": 0 },
                "reason": { "type": ["string", "null"] }
            }
        },
        "ModBanRequest": {
            "type": "object",
            "required": ["user"],
            "properties": {
                "user": {
                    "type": "object",
                    "required": ["platform", "user_id", "login"],
                    "properties": {
                        "platform": { "type": "string" },
                        "user_id": { "type": "string" },
                        "login": { "type": "string" },
                        "display_name": { "type": ["string", "null"] }
                    }
                },
                "reason": { "type": ["string", "null"], "maxLength": 500 },
                "everywhere": { "type": "boolean", "description": "Ban on every connected platform, not only this one." }
            }
        },
        "BanResult": {
            "type": "object",
            "properties": {
                "banned": { "type": "array", "items": { "type": "string" }, "description": "Platforms the ban went through on." },
                "failed": { "type": "array", "items": { "type": "object" }, "description": "Platforms it didn't, with why." }
            }
        },
        "ModUnbanRequest": {
            "type": "object",
            "required": ["platform", "user_id"],
            "properties": {
                "platform": { "type": "string" },
                "user_id": { "type": "string" }
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
    });
    if let Some(map) = doc.as_object_mut() {
        map.extend(studio_source_schemas());
        map.extend(studio_stats_schemas());
        map.extend(auto_config_schemas());
        map.extend(golive_quality_schemas());
        map.extend(store_schemas());
        map.extend(schedule_schemas());
        map.extend(links_and_labels_schemas());
    }
    doc
}

fn links_and_labels_schemas() -> Map<String, Value> {
    let mut m = Map::new();
    let opt_str = json!({ "type": ["string", "null"], "maxLength": 120 });
    m.insert(
        "CustomLink".into(),
        json!({
            "type": "object",
            "required": ["label", "value"],
            "properties": {
                "label": { "type": "string", "maxLength": 40 },
                "value": { "type": "string", "maxLength": 120 }
            }
        }),
    );
    m.insert(
        "CreatorLinks".into(),
        json!({
            "type": "object",
            "description": "Creator \"My links\" configuration. On PUT /api/v1/links, omitted fields and unknown future keys are preserved; an explicit empty string or null clears a string field, and [] or null clears custom.",
            "properties": {
                "twitch": opt_str,
                "youtube": opt_str,
                "kick": opt_str,
                "facebook": opt_str,
                "discord": opt_str,
                "instagram": opt_str,
                "x": opt_str,
                "tiktok": opt_str,
                "website": opt_str,
                "schedule_text": opt_str,
                "custom": { "type": ["array", "null"], "maxItems": 6, "items": { "$ref": "#/components/schemas/CustomLink" } }
            }
        }),
    );
    m.insert(
        "ResolvedLinks".into(),
        json!({
            "type": "object",
            "required": ["custom", "socials"],
            "properties": {
                "twitch": opt_str,
                "youtube": opt_str,
                "kick": opt_str,
                "facebook": opt_str,
                "discord": opt_str,
                "instagram": opt_str,
                "x": opt_str,
                "tiktok": opt_str,
                "website": opt_str,
                "custom": { "type": "array", "items": { "$ref": "#/components/schemas/CustomLink" } },
                "socials": { "type": "array", "items": { "type": "string" } },
                "channel_name": { "type": ["string", "null"] },
                "next_stream": { "type": ["string", "null"] },
                "next_stream_at": { "type": ["integer", "null"] }
            }
        }),
    );
    m.insert(
        "LinksResponse".into(),
        json!({
            "type": "object",
            "required": ["custom", "resolved"],
            "properties": {
                "twitch": opt_str,
                "youtube": opt_str,
                "kick": opt_str,
                "facebook": opt_str,
                "discord": opt_str,
                "instagram": opt_str,
                "x": opt_str,
                "tiktok": opt_str,
                "website": opt_str,
                "schedule_text": opt_str,
                "custom": { "type": "array", "maxItems": 6, "items": { "$ref": "#/components/schemas/CustomLink" } },
                "resolved": { "$ref": "#/components/schemas/ResolvedLinks" }
            }
        }),
    );
    m.insert(
        "LabelsMap".into(),
        json!({
            "type": "object",
            "description": "Evaluated stream label display strings keyed by LabelSource snake_case name.",
            "additionalProperties": { "type": "string" }
        }),
    );
    m
}

fn studio_source_schemas() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "StudioSourceStill".into(),
        json!({
            "type": "object",
            "description": "Picker thumbnail response for a display, window, or camera.",
            "required": ["image"],
            "properties": {
                "image": { "type": ["string", "null"] },
                "waiting": {
                    "type": "boolean",
                    "description": "Present and true only when a camera is temporarily disconnected or waiting for its first frame."
                },
                "error": { "type": ["string", "null"] }
            }
        }),
    );
    m.insert(
        "StudioSourceScreenshotRequest".into(),
        json!({
            "type": "object",
            "description": "Optional scene-item target. Provide both `scene` and `item` (here or as query parameters; omit both to capture the raw source).",
            "properties": {
                "scene": { "type": "string", "description": "Scene name holding `item`." },
                "item": { "type": "integer", "description": "Scene item id in `scene`." }
            }
        }),
    );
    m.insert(
        "StudioScreenshotResponse".into(),
        json!({
            "type": "object",
            "description": "Saved screenshot file result.",
            "required": ["file"],
            "properties": {
                "file": { "type": "string", "description": "Full local path of the saved PNG screenshot." }
            }
        }),
    );
    m.insert(
        "StudioSourceStatus".into(),
        json!({
            "type": "object",
            "description": "Live health/status details for a source (`{}` when the source kind reports no live status).",
            "properties": {
                "camera": {
                    "type": "object",
                    "description": "Present for open camera/capture sources.",
                    "required": ["state", "tries"],
                    "properties": {
                        "state": { "type": "string", "enum": ["live", "waiting"] },
                        "message": { "type": ["string", "null"] },
                        "tries": { "type": "integer", "minimum": 0 }
                    }
                },
                "web_text": {
                    "type": ["object", "null"],
                    "properties": {
                        "at_ms": { "type": "integer", "minimum": 0 },
                        "value": { "type": ["string", "null"] },
                        "error": { "type": ["string", "null"] }
                    }
                },
                "page": {
                    "type": "object",
                    "required": ["ok"],
                    "properties": {
                        "ok": { "type": "boolean" },
                        "error": { "type": ["string", "null"] }
                    }
                },
                "app_on": {
                    "type": "array",
                    "items": { "type": "integer" }
                }
            }
        }),
    );
    m
}

fn studio_stats_schemas() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "CaptureProblem".into(),
        json!({
            "type": "object",
            "description": "Active capture source on air showing no usable picture.",
            "required": ["source", "kind", "problem"],
            "properties": {
                "source": { "type": "string" },
                "kind": { "type": "string" },
                "problem": {
                    "type": "string",
                    "enum": ["permission", "waiting", "no_picture", "black"]
                }
            }
        }),
    );
    m.insert(
        "StudioEngineStats".into(),
        json!({
            "type": "object",
            "description": "Compositor metrics, active capture issues, output status, and per-source frame load.",
            "required": ["engine", "scene", "history", "canvas", "disk_free_mb", "captures", "sources"],
            "properties": {
                "engine": {
                    "type": "object",
                    "properties": {
                        "cpu": { "type": "number" },
                        "fps": { "type": "number" },
                        "frame_ms": { "type": "number" },
                        "lagged": { "type": "integer" },
                        "frames": { "type": "integer" },
                        "memory_mb": { "type": "integer" },
                        "skipped": { "type": "integer" },
                        "encoded": { "type": "integer" },
                        "recovered": { "type": "integer" },
                        "restarts": { "type": "integer" },
                        "encode_ms": { "type": "number" },
                        "encoding_ms": { "type": "integer" }
                    }
                },
                "scene": { "type": ["string", "null"] },
                "history": {
                    "type": "object",
                    "properties": {
                        "undo": { "type": ["string", "null"] },
                        "redo": { "type": ["string", "null"] }
                    }
                },
                "canvas": {
                    "type": "object",
                    "properties": {
                        "width": { "type": "integer" },
                        "height": { "type": "integer" },
                        "fps": { "type": "integer" }
                    }
                },
                "stream": { "type": ["object", "null"] },
                "record": { "type": ["object", "null"] },
                "source_records": { "type": "array", "items": { "type": "object" } },
                "disk_free_mb": { "type": "integer" },
                "replay": { "type": ["object", "null"] },
                "virtualcam": { "type": ["object", "null"] },
                "ndi": { "type": ["object", "null"] },
                "captures": {
                    "type": "array",
                    "items": { "$ref": "#/components/schemas/CaptureProblem" }
                },
                "sources": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["name", "sent", "waiting"],
                        "properties": {
                            "name": { "type": "string" },
                            "sent": { "type": "integer" },
                            "width": { "type": ["integer", "null"] },
                            "height": { "type": ["integer", "null"] },
                            "waiting": { "type": "boolean" }
                        }
                    }
                },
                "remux": { "type": ["object", "null"] },
                "sidechain_cycles": { "type": "array", "items": { "type": "object" } }
            }
        }),
    );
    m
}

/// A frame of the live socket. The `type` constant is the wire discriminator: with it a frame
/// matches exactly one message (without it `{"type":"brb"}` would match every message at once).
fn live_message(name: &str, summary: &str, scope_note: &str, data_schema: Value) -> Value {
    json!({
        "summary": summary,
        "x-ferrocast-scopes": scope_note,
        "payload": {
            "type": "object",
            "required": ["v", "type", "data"],
            "properties": {
                "v": { "type": "integer", "const": 1 },
                "type": { "type": "string", "const": name },
                "seq": { "type": ["integer", "null"] },
                "data": data_schema
            }
        }
    })
}

/// A frame the server sends an overlay: numbered, `{ v, seq, type, data }`.
fn overlay_message(name: &str, summary: &str, data_schema: Value) -> Value {
    json!({
        "summary": summary,
        "payload": {
            "type": "object",
            "required": ["v", "seq", "type", "data"],
            "properties": {
                "v": { "type": "integer", "const": 1 },
                "seq": { "type": "integer" },
                "type": { "type": "string", "const": name },
                "data": data_schema
            }
        }
    })
}

/// A frame an overlay sends the server: `{ type, data }`, as `ws.rs` reads it.
fn overlay_inbound(name: &str, summary: &str, data_schema: Value) -> Value {
    json!({
        "summary": summary,
        "payload": {
            "type": "object",
            "required": ["type", "data"],
            "properties": {
                "type": { "type": "string", "const": name },
                "data": data_schema
            }
        }
    })
}

fn build_asyncapi() -> Value {
    let mut doc = json!({
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
                "bindings": {
                    "ws": {
                        "bindingVersion": "0.1.0",
                        "method": "GET",
                        "query": {
                            "type": "object",
                            "description": "Handshake query. Send `key` (an API key) or `session`; `since` resumes a dropped connection.",
                            "properties": {
                                "since": { "type": "integer", "minimum": 0, "description": "Optional last processed `seq` for lossless reconnect replay." },
                                "key": { "type": "string", "description": "API key when header auth isn't available (prefer `Authorization: Bearer fc_key_...`). Only on a connection from this machine, unless the owner allowed API keys over LAN." },
                                "session": { "type": "string", "description": "Browser session token." }
                            }
                        },
                        "headers": {
                            "type": "object",
                            "properties": {
                                "Authorization": { "type": "string", "description": "`Bearer fc_key_...` (recommended)." }
                            }
                        }
                    }
                },
                "messages": {
                    "hello": live_message("hello", "Initial handshake with highest sequence number (`{ seq }`).", "read_only, chat, alerts, studio_control, go_live", json!({ "type": "object", "properties": { "seq": { "type": "integer" } } })),
                    "resync": live_message("resync", "Ring buffer gap notice (`{ missed }`); clients should re-fetch state over REST.", "read_only, chat, alerts, studio_control, go_live", json!({ "type": "object", "properties": { "missed": { "type": "integer" } } })),
                    "status": live_message("status", "Connector and OBS health snapshots.", "read_only, chat, alerts, studio_control, go_live", json!({ "type": "object", "properties": { "connectors": { "type": "object" } } })),
                    "queue": live_message("queue", "Alert queue snapshot (active/paused/muted/pending count).", "read_only, alerts", json!({ "type": "object" })),
                    "event": live_message("event", "Normalized stream event (follow, sub, gift_sub, tip, bits, raid, chat_message, etc.).", "read_only; chat (chat/mod kinds); alerts (stream events)", json!({ "type": "object", "properties": { "id": { "type": "string" }, "platform": { "type": "string" }, "kind": { "type": "object" } } })),
                    "notice": live_message("notice", "Engine status notice or panic warning (`account:*` notices suppressed for API keys).", "read_only, alerts, studio_control, go_live (chat sees panic only)", json!({ "type": "object", "properties": { "level": { "type": "string" }, "message": { "type": "string" }, "key": { "type": ["string", "null"] } } })),
                    "goals": live_message("goals", "Updated stream goals array.", "read_only, alerts", json!({ "type": "array" })),
                    "links": live_message("links", "Updated \"My links\" and resolved overlay link state.", "read_only, alerts, settings", json!({ "type": "object" })),
                    "held": live_message("held", "Message/event held by safety filters (`{ event_id, reason }`).", "read_only, chat", json!({ "type": "object" })),
                    "held_resolved": live_message("held_resolved", "Held event released or dropped (`{ event_id, outcome }`).", "read_only, chat", json!({ "type": "object" })),
                    "panic": live_message("panic", "Panic lockdown toggled (`{ on, plan }`).", "read_only, chat", json!({ "type": "object" })),
                    "chatter": live_message("chatter", "Updated tracked chatter metadata.", "read_only, chat", json!({ "type": "object" })),
                    "chat_highlight": live_message("chat_highlight", "Pinned or cleared featured chat message.", "read_only, chat", json!({ "type": "object" })),
                    "poll": live_message("poll", "Active poll progress or end state.", "read_only, chat", json!({ "type": "object" })),
                    "giveaway": live_message("giveaway", "Active giveaway state, entry count, or winner.", "read_only, chat", json!({ "type": "object" })),
                    "tts_pending": live_message("tts_pending", "TTS queue items awaiting approval.", "read_only, chat, alerts", json!({ "type": "array" })),
                    "golive": live_message(
                        "golive",
                        "Multistream output state, per-destination resolved quality, encode groups/budget, and live bitrate telemetry.",
                        "read_only, go_live, studio_control",
                        golive_live_frame_schema(),
                    ),
                    "smart_mod": live_message("smart_mod", "Smart Moderation triage card.", "read_only, chat", json!({ "type": "object" })),
                    "update": live_message("update", "Application update check/download state.", "read_only, studio_control", json!({ "type": "object" })),
                    "account": live_message("account", "Ferrocast cloud account sign-in transition (PC session only, never sent to API keys).", "session only (blocked for API keys)", json!({ "type": "object" })),
                    "brb": live_message("brb", "OBS BRB scene state (`{ on, scene }`).", "read_only, studio_control, go_live", json!({ "type": "object" })),
                    "obs_health": live_message("obs_health", "OBS FPS/dropped-frames health snapshot.", "read_only, studio_control, go_live", json!({ "type": "object" })),
                    "live_alert": live_message("live_alert", "Live stream encoder/audio/network alert banner.", "read_only, studio_control, go_live", json!({ "type": "object" })),
                    "lane": live_message("lane", "Live Moments lane items.", "read_only, studio_control, go_live", json!({ "type": "object" })),
                    "marker": live_message("marker", "New stream marker created.", "read_only, studio_control, go_live", json!({ "type": "object" })),
                    "counter": live_message("counter", "Updated bot counter (`{ name, value }`).", "read_only, chat, studio_control", json!({ "type": "object" })),
                    "show": live_message("show", "Show countdown and live rundown state (`{ show }`).", "read_only, studio_control, go_live", json!({ "type": "object" })),
                    "auto_config": live_message("auto_config", "Encoder auto-config progress and result: the same `AutoConfigView` the `/studio/engine/auto-config` routes return. Sent on connect only while a run is not idle.", "PC session and the owner's remote only; never sent to API keys", json!({ "$ref": "#/components/schemas/AutoConfigView" })),
                    "snapshot": live_message("snapshot", "Overlay reconnect full state snapshot.", "overlay channel only", json!({ "type": "object" }))
                }
            },
            "overlay": {
                "address": "/o/{token}/ws",
                "description": "Overlay browser-source WebSocket carrying widget snapshots, alerts, music visualizer frames, and display acks.",
                "parameters": {
                    "token": { "description": "Opaque overlay URL token." }
                },
                "messages": {
                    "snapshot": overlay_message("snapshot", "Initial full overlay config + widget state on connect.", json!({ "type": "object" })),
                    "config": overlay_message("config", "Updated overlay config.", json!({ "type": "object" })),
                    "state": overlay_message("state", "Updated widget/goal/poll/giveaway/credits state.", json!({ "type": "object" })),
                    "event": overlay_message("event", "Public chat/event card frame.", json!({ "type": "object" })),
                    "alert": overlay_message("alert", "Show alert visual & sound frame.", json!({ "type": "object" })),
                    "alert_hide": overlay_message("alert_hide", "Dismiss active alert frame (`{ alert_id }`).", json!({ "type": "object" })),
                    "tts": overlay_message("tts", "Play synthesized TTS audio in overlay.", json!({ "type": "object" })),
                    "music": overlay_message("music", "Visualizer spectrum levels & track info.", json!({ "type": "object" })),
                    "delete_message": overlay_message("delete_message", "Remove moderated chat message from overlay chatbox.", json!({ "type": "object" })),
                    "delete_user": overlay_message("delete_user", "Remove all messages from a timed-out/banned user.", json!({ "type": "object" })),
                    "clear_chat": overlay_message("clear_chat", "Clear overlay chatbox history.", json!({ "type": "object" })),
                    "revoked": overlay_message("revoked", "Overlay token was rotated or deleted; client should stop reconnecting.", json!({ "type": "object" })),
                    "inbound_hello": overlay_inbound("hello", "Client -> server on connect: where to resume from (`{ since_seq }`).", json!({ "type": "object", "properties": { "since_seq": { "type": ["integer", "null"] } } })),
                    "inbound_ack": overlay_inbound("ack", "Client -> server alert playback acknowledgment: `{ type: 'ack', data: { alert_id, phase } }`.", json!({ "type": "object", "required": ["alert_id", "phase"], "properties": { "alert_id": { "type": "string" }, "phase": { "type": "string", "enum": ["started", "finished", "failed"] } } })),
                    "inbound_visibility": overlay_inbound("visibility", "Client -> server browser source visibility telemetry: `{ type: 'visibility', data: { visible } }`.", json!({ "type": "object", "required": ["visible"], "properties": { "visible": { "type": "boolean" } } }))
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
    });
    // A frame's data may point at these (`#/components/schemas/...`), the same schemas the REST
    // side uses, so the two documents cannot say different things about one object.
    let all = schemas();
    let mut shared = Map::new();
    for name in [
        "AutoConfigView",
        "AutoConfigCandidate",
        "AutoConfigRecommendation",
        "AutoConfigUploadCheck",
        "AutoConfigAppliedSummary",
        "DestinationQuality",
        "ResolvedQuality",
        "EncodeGroupView",
        "EncodeBudgetCheck",
        "DestinationView",
    ] {
        if let Some(def) = all.get(name) {
            shared.insert(name.to_string(), def.clone());
        }
    }
    doc["components"] = json!({ "schemas": shared });
    doc
}

fn golive_live_frame_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "live": { "type": "boolean" },
            "stats": { "type": "object" },
            "vertical_live": { "type": "boolean" },
            "vertical_stats": { "type": "object" },
            "destinations": { "type": "array", "items": { "$ref": "#/components/schemas/DestinationView" } },
            "encode_groups": { "type": "array", "items": { "$ref": "#/components/schemas/EncodeGroupView" } },
            "encode_budget": { "$ref": "#/components/schemas/EncodeBudgetCheck" },
            "cut_state": { "type": ["object", "string", "null"] },
            "ffmpeg": { "type": ["object", "string", "null"] }
        }
    })
}
