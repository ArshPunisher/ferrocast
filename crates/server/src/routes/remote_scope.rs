//! What a paired phone may reach on this PC (Pro F2, API.md §9.5), checked for every
//! request that comes through the relay's tunnel (`crate::remote`), after the relay checked
//! the same rules (`crates/relay/src/remote/scope.rs`; a test keeps the two copies equal).
//!
//! Paths are relative to `/api/v1` (`/mod/ban`). A path that isn't plain (`..`, `//`, an
//! encoded slash or dot, control characters) is refused for every role.
//!
//! - `owner`: every dashboard route except accounts and sign-ins, secrets (tip provider
//!   secrets, the Discord webhook, the OBS password), Pro and billing, export and import,
//!   restoring a backup, installing an update and media uploads. **A new route is open to
//!   owners unless it's added to [`OWNER_DENIED`] here and on the relay.**
//! - `mod`: chat (read), ban, timeout, delete, unban, held messages (approve or remove),
//!   panic on and off, and the viewer card. Nothing else, ever, without adding it here.

use std::fmt;

/// A paired device's role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Owner,
    Mod,
}

impl Role {
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "owner" => Some(Role::Owner),
            "mod" => Some(Role::Mod),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Mod => "mod",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `*` matches one segment, a trailing `**` any number (one or more).
type Rule = (&'static str, &'static str);

/// Everything a mod may do. Methods are exact.
pub const MOD_ALLOWED: &[Rule] = &[
    ("GET", "/status"),
    ("GET", "/chat"),
    ("POST", "/mod/delete"),
    ("POST", "/mod/timeout"),
    ("POST", "/mod/ban"),
    ("POST", "/mod/unban"),
    ("GET", "/mod/bans"),
    ("GET", "/mod/held"),
    ("POST", "/mod/held/*/release"),
    ("POST", "/mod/held/*/drop"),
    ("GET", "/mod/panic"),
    ("POST", "/mod/panic"),
    ("GET", "/viewers/*/*"),
    ("GET", "/viewers/*/*/note"),
];

/// Exceptions inside [`OWNER_DENIED`] an owner's phone may still use: reading the weekly
/// email's state (the address only as a hint). Changing the address stays on the PC.
pub const OWNER_ALLOWED: &[Rule] = &[
    ("GET", "/pro/history/email"),
    // Stream history (Pro F7): read it from the phone; the relay answers it while the PC is off.
    ("GET", "/pro/history"),
    // The cloud bot (Pro F1): look at it and switch it on or off; connecting stays on the PC.
    ("GET", "/pro/bot"),
    ("PUT", "/pro/bot/enabled"),
    // The tip page's own address (Pro F6): its status only; adding or removing stays on the PC.
    ("GET", "/pro/tips/domain"),
];

/// What an owner's phone may never do (`*` as the method: any method).
pub const OWNER_DENIED: &[Rule] = &[
    // The Studio's engine runs in the desktop window on the PC: a phone can't place its preview
    // there or start and stop it.
    ("*", "/studio/engine"),
    ("*", "/studio/engine/**"),
    // Accounts and sign-ins, and pairing more browsers or managing local API keys.
    ("*", "/accounts"),
    ("*", "/accounts/**"),
    ("*", "/session/**"),
    ("*", "/api-keys"),
    ("*", "/api-keys/**"),
    // Pro and billing (including pairing more phones and the cloud backups' key).
    ("*", "/pro"),
    ("*", "/pro/**"),
    // Secrets: tip provider secrets, the Discord webhook, the OBS password.
    ("PUT", "/tips"),
    ("DELETE", "/tips"),
    ("*", "/discord"),
    ("*", "/discord/**"),
    ("PUT", "/obs"),
    ("*", "/obs/detect"),
    // Export, import and restore.
    ("*", "/export"),
    ("*", "/import"),
    ("*", "/import/**"),
    ("*", "/designs/import"),
    // Plugins bring code (effects): installed and managed on the PC only.
    ("*", "/plugins"),
    ("*", "/plugins/**"),
    ("*", "/designs/*/export"),
    ("*", "/backups/*/restore"),
    // Media uploads (the tip page's avatar and banner too).
    ("POST", "/media"),
    ("*", "/tips/brand/*"),
    // Checking, installing or restarting for an update from a phone (reading it is fine).
    ("*", "/updates/*"),
    // The live socket has its own route on the relay, never a request frame.
    ("*", "/live"),
    // Go live: the local stream key, and adding or removing a server with its key, stay on
    // the PC. Watching, ticking platforms and the checks are fine from a phone. Trying an
    // address that isn't saved stays on the PC too: a phone mustn't make it reach any host
    // (a saved server's own test is fine). Nor does the list of its networks and addresses.
    ("*", "/golive/ingest"),
    ("POST", "/golive/destinations"),
    ("*", "/golive/destinations/*"),
    ("*", "/golive/test"),
    ("*", "/golive/networks"),
    // Support diagnostics inspect local machine logs, hardware, and crash dumps: PC only.
    ("*", "/diagnostics"),
    ("*", "/diagnostics/**"),
];

/// One plain path segment: no `.`/`..`, no encoded `/`, `\`, `.` or NUL, nothing outside
/// printable ASCII.
fn segment_ok(s: &str) -> bool {
    if s.is_empty() || s == "." || s == ".." || s.len() > 200 {
        return false;
    }
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'%' {
            let (Some(h), Some(l)) = (b.get(i + 1), b.get(i + 2)) else { return false };
            if !h.is_ascii_hexdigit() || !l.is_ascii_hexdigit() {
                return false;
            }
            let v = u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap_or(0);
            if v < 0x20 || matches!(v, b'/' | b'\\' | b'.' | 0x7f) {
                return false;
            }
            i += 3;
            continue;
        }
        if !(c.is_ascii_alphanumeric() || b"-_.~:@!$&'()*+,;=".contains(&c)) {
            return false;
        }
        i += 1;
    }
    true
}

/// The segments of a plain API path (`/mod/ban` → `["mod", "ban"]`), or `None`.
pub fn segments(path: &str) -> Option<Vec<&str>> {
    let rest = path.strip_prefix('/')?;
    if rest.is_empty() || path.len() > 512 {
        return None;
    }
    let segs: Vec<&str> = rest.split('/').collect();
    segs.iter().all(|s| segment_ok(s)).then_some(segs)
}

fn matches(pattern: &str, segs: &[&str]) -> bool {
    let pat: Vec<&str> = pattern.trim_start_matches('/').split('/').collect();
    if pat.last() == Some(&"**") {
        let fixed = &pat[..pat.len() - 1];
        return segs.len() > fixed.len() && fixed.iter().zip(segs).all(|(p, s)| *p == "*" || p == s);
    }
    pat.len() == segs.len() && pat.iter().zip(segs).all(|(p, s)| *p == "*" || p == s)
}

fn method_is(rule: &str, method: &str) -> bool {
    rule == "*" || rule.eq_ignore_ascii_case(method)
}

/// Whether `role` may call `method path` (path relative to `/api/v1`, no query).
pub fn allowed(role: Role, method: &str, path: &str) -> bool {
    let Some(segs) = segments(path) else { return false };
    if !matches!(method, "GET" | "POST" | "PUT" | "DELETE") {
        return false;
    }
    match role {
        Role::Mod => MOD_ALLOWED.iter().any(|(m, p)| method_is(m, method) && matches(p, &segs)),
        Role::Owner => {
            OWNER_ALLOWED.iter().any(|(m, p)| method_is(m, method) && matches(p, &segs))
                || !OWNER_DENIED.iter().any(|(m, p)| method_is(m, method) && matches(p, &segs))
        }
    }
}

/// Event kinds a mod's live socket gets (`event` frames): chat and what mods do to it.
pub const MOD_EVENT_KINDS: &[&str] = &["chat_message", "message_deleted", "user_messages_cleared", "ban"];

/// Live socket frame types a mod gets; owners get every frame.
pub const MOD_FRAMES: &[&str] = &["hello", "resync", "status", "event", "held", "held_resolved", "panic", "chatter"];

/// Whether a live frame (`{ v, type, data }`, as text) may go to `role`.
pub fn live_frame_allowed(role: Role, frame: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(frame) else { return role == Role::Owner };
    let ty = v["type"].as_str().unwrap_or("");
    // The Ferrocast account (email, sessions) stays on the PC, like its `/pro/account` routes.
    if ty == "account" {
        return false;
    }
    if role == Role::Owner {
        return true;
    }
    match ty {
        "event" => v.pointer("/data/kind/type").and_then(|t| t.as_str()).is_some_and(|k| MOD_EVENT_KINDS.contains(&k)),
        // Only the panic notice; others can name accounts, files or settings.
        "notice" => v.pointer("/data/key").and_then(|k| k.as_str()) == Some("panic"),
        t => MOD_FRAMES.contains(&t),
    }
}

/// Endpoints that local API keys may NEVER access, regardless of which scopes are granted.
pub const KEY_NEVER_ALLOWED: &[Rule] = &[
    // Managing other API keys or browser/phone pairing sessions.
    ("*", "/api-keys"),
    ("*", "/api-keys/**"),
    ("*", "/session"),
    ("*", "/session/**"),
    // Connected platform accounts & Ferrocast account / billing / remote tunnel / cloud services.
    ("*", "/accounts"),
    ("*", "/accounts/**"),
    ("*", "/pro"),
    ("*", "/pro/**"),
    // Secrets & external webhooks/passwords/tips configuration.
    ("*", "/tips"),
    ("*", "/tips/**"),
    ("*", "/discord"),
    ("*", "/discord/**"),
    ("PUT", "/obs"),
    ("*", "/obs/detect"),
    // Diagnostics exports & bundles.
    ("*", "/diagnostics"),
    ("*", "/diagnostics/**"),
    // Plugin catalog, install, toggle, actions, removal.
    ("*", "/plugins"),
    ("*", "/plugins/**"),
    // Arbitrary pack exports, third-party imports, backup restores, updates, uploads.
    ("*", "/export"),
    ("*", "/import"),
    ("*", "/import/**"),
    ("*", "/designs/import"),
    ("*", "/designs/*/export"),
    ("*", "/backups/*/restore"),
    ("POST", "/media"),
    ("*", "/updates/*"),
    // Go live ingest stream keys, creating/deleting destinations, SSRF probes, LAN interfaces.
    ("*", "/golive/ingest"),
    ("POST", "/golive/destinations"),
    ("DELETE", "/golive/destinations/*"),
    ("*", "/golive/test"),
    ("*", "/golive/networks"),
    ("POST", "/golive/new-key"),
    // Studio remote-control password, guest invite tokens, local filesystem/log/restart/capture paths.
    ("*", "/studio/remote-control"),
    ("*", "/studio/remote-control/**"),
    ("*", "/studio/guests"),
    ("*", "/studio/guests/**"),
    ("*", "/studio/engine/log"),
    ("*", "/studio/engine/log/**"),
    ("*", "/studio/engine/pick-file"),
    ("*", "/studio/engine/web-window"),
    ("*", "/studio/engine/restart"),
    ("*", "/studio/engine/missing-files"),
    ("*", "/studio/engine/missing-files/**"),
    ("*", "/studio/engine/obs-collections"),
    ("*", "/studio/engine/obs-collections/**"),
    ("*", "/studio/engine/streamlabs-collections"),
    ("*", "/studio/engine/streamlabs-collections/**"),
    ("*", "/studio/engine/library/*/import"),
    ("*", "/studio/engine/library/*/export"),
    ("*", "/studio/engine/library/*/*/export"),
    ("*", "/studio/engine/library/collections/overlay/**"),
    ("*", "/studio/engine/screen-access/fix"),
    ("*", "/studio/engine/remux"),
    ("*", "/studio/engine/keys/capture"),
    ("POST", "/speech/models/*"),
    ("DELETE", "/speech/models/*"),
];

const KEY_COMMON_READ: &[Rule] = &[("GET", "/status"), ("GET", "/openapi.json"), ("GET", "/asyncapi.json")];

pub const READ_ONLY_ALLOWED: &[Rule] = &[
    ("GET", "/live"),
    ("GET", "/themes"),
    ("GET", "/updates"),
    ("GET", "/stats/session"),
    ("GET", "/recap"),
    ("GET", "/recap/summary"),
    ("GET", "/credits"),
    ("GET", "/insights/supporters"),
    ("GET", "/overlays"),
    ("GET", "/overlays/*"),
    ("GET", "/designs"),
    ("GET", "/designs/active-alert"),
    ("GET", "/designs/*"),
    ("GET", "/designs/*/obs"),
    ("GET", "/alert-rules"),
    ("GET", "/queue"),
    ("GET", "/events"),
    ("GET", "/events/*/why"),
    ("GET", "/goals"),
    ("GET", "/chat"),
    ("GET", "/chat/highlight"),
    ("GET", "/mod/bans"),
    ("GET", "/mod/panic"),
    ("GET", "/mod/panic/config"),
    ("GET", "/mod/held"),
    ("GET", "/mod/filters"),
    ("GET", "/viewers/*/*"),
    ("GET", "/viewers/*/*/note"),
    ("GET", "/commands"),
    ("GET", "/timers"),
    ("GET", "/autoreplies"),
    ("GET", "/autoreplies/starters"),
    ("GET", "/loyalty"),
    ("GET", "/loyalty/leaderboard"),
    ("GET", "/loyalty/viewers/*/*"),
    ("GET", "/polls/current"),
    ("GET", "/giveaways/current"),
    ("GET", "/welcome"),
    ("GET", "/tts"),
    ("GET", "/tts/pending"),
    ("GET", "/tts/voices"),
    ("GET", "/tts/cloud"),
    ("GET", "/media"),
    ("GET", "/media/*"),
    ("GET", "/music"),
    ("GET", "/obs"),
    ("GET", "/obs/theme"),
    ("GET", "/obs/scenes"),
    ("GET", "/obs/inputs"),
    ("GET", "/obs/stream"),
    ("GET", "/obs/health"),
    ("GET", "/obs/switcher"),
    ("GET", "/golive"),
    ("GET", "/golive/picture"),
    ("GET", "/studio/engine"),
    ("GET", "/studio/engine/**"),
    ("GET", "/speech/models"),
    ("GET", "/transcript"),
    ("GET", "/counters"),
    ("GET", "/quotes"),
    ("GET", "/modules"),
    ("GET", "/stream-info"),
    ("GET", "/stream-info/categories"),
    ("GET", "/stream-info/categories/as"),
    ("GET", "/show"),
    ("GET", "/show/config"),
    ("GET", "/show/preflight"),
    ("GET", "/show/alerts"),
    ("GET", "/moments/config"),
    ("GET", "/moments/lane"),
    ("GET", "/moments/markers"),
    ("GET", "/moments/markers/export"),
    ("GET", "/automations"),
    ("GET", "/settings"),
    ("GET", "/backups"),
];

pub const CHAT_ALLOWED: &[Rule] = &[
    ("GET", "/live"),
    ("GET", "/chat"),
    ("POST", "/chat/send"),
    ("GET", "/chat/highlight"),
    ("POST", "/chat/highlight"),
    ("DELETE", "/chat/highlight"),
    ("POST", "/mod/delete"),
    ("POST", "/mod/timeout"),
    ("POST", "/mod/ban"),
    ("POST", "/mod/unban"),
    ("GET", "/mod/bans"),
    ("GET", "/mod/panic"),
    ("POST", "/mod/panic"),
    ("GET", "/mod/panic/config"),
    ("PUT", "/mod/panic/config"),
    ("GET", "/mod/held"),
    ("POST", "/mod/held/*/release"),
    ("POST", "/mod/held/*/drop"),
    ("GET", "/mod/filters"),
    ("PUT", "/mod/filters"),
    ("GET", "/viewers/*/*"),
    ("GET", "/viewers/*/*/note"),
    ("PUT", "/viewers/*/*/note"),
    ("GET", "/commands"),
    ("POST", "/commands"),
    ("POST", "/commands/starter"),
    ("PUT", "/commands/*"),
    ("DELETE", "/commands/*"),
    ("GET", "/timers"),
    ("POST", "/timers"),
    ("PUT", "/timers/*"),
    ("DELETE", "/timers/*"),
    ("GET", "/autoreplies"),
    ("POST", "/autoreplies"),
    ("PUT", "/autoreplies/order"),
    ("POST", "/autoreplies/try"),
    ("GET", "/autoreplies/starters"),
    ("PUT", "/autoreplies/*"),
    ("DELETE", "/autoreplies/*"),
    ("GET", "/loyalty"),
    ("PUT", "/loyalty"),
    ("GET", "/loyalty/leaderboard"),
    ("GET", "/loyalty/viewers/*/*"),
    ("POST", "/loyalty/viewers/*/*/adjust"),
    ("POST", "/polls"),
    ("GET", "/polls/current"),
    ("DELETE", "/polls/current"),
    ("POST", "/polls/current/end"),
    ("POST", "/giveaways"),
    ("GET", "/giveaways/current"),
    ("DELETE", "/giveaways/current"),
    ("POST", "/giveaways/current/*"),
    ("GET", "/welcome"),
    ("PUT", "/welcome"),
    ("GET", "/tts"),
    ("PUT", "/tts"),
    ("GET", "/tts/pending"),
    ("GET", "/tts/voices"),
    ("GET", "/tts/cloud"),
    ("PUT", "/tts/cloud"),
    ("POST", "/tts/preview"),
    ("POST", "/tts/*/approve"),
    ("POST", "/tts/*/reject"),
    ("GET", "/counters"),
    ("POST", "/counters"),
    ("PUT", "/counters/*"),
    ("DELETE", "/counters/*"),
    ("POST", "/counters/*/change"),
    ("GET", "/quotes"),
    ("POST", "/quotes"),
    ("PUT", "/quotes/*"),
    ("DELETE", "/quotes/*"),
    ("GET", "/modules"),
    ("PUT", "/modules"),
];

pub const ALERTS_ALLOWED: &[Rule] = &[
    ("GET", "/live"),
    ("GET", "/overlays"),
    ("POST", "/overlays"),
    ("GET", "/overlays/*"),
    ("PUT", "/overlays/*"),
    ("DELETE", "/overlays/*"),
    ("POST", "/overlays/*/regenerate-url"),
    ("POST", "/overlays/*/add-to-obs"),
    ("POST", "/overlays/*/follow-design"),
    ("GET", "/designs"),
    ("POST", "/designs"),
    ("GET", "/designs/active-alert"),
    ("PUT", "/designs/active-alert"),
    ("GET", "/designs/*"),
    ("PUT", "/designs/*"),
    ("DELETE", "/designs/*"),
    ("POST", "/designs/*/duplicate"),
    ("POST", "/designs/*/use"),
    ("GET", "/alert-rules"),
    ("POST", "/alert-rules"),
    ("PUT", "/alert-rules/*"),
    ("DELETE", "/alert-rules/*"),
    ("POST", "/test"),
    ("GET", "/queue"),
    ("POST", "/queue/*"),
    ("POST", "/queue/*/remove"),
    ("GET", "/events"),
    ("POST", "/events/*/replay"),
    ("GET", "/events/*/why"),
    ("GET", "/goals"),
    ("POST", "/goals"),
    ("PUT", "/goals/*"),
    ("DELETE", "/goals/*"),
    ("POST", "/goals/*/adjust"),
    ("GET", "/media"),
    ("GET", "/media/*"),
    ("DELETE", "/media/*"),
];

pub const STUDIO_CONTROL_ALLOWED: &[Rule] = &[
    ("GET", "/live"),
    ("GET", "/studio/engine"),
    ("*", "/studio/engine/**"),
    ("GET", "/obs"),
    ("GET", "/obs/theme"),
    ("DELETE", "/obs/theme"),
    ("GET", "/designs/*/obs"),
    ("POST", "/designs/*/obs"),
    ("GET", "/obs/scenes"),
    ("GET", "/obs/inputs"),
    ("POST", "/obs/scene"),
    ("POST", "/obs/source"),
    ("POST", "/obs/input/mute"),
    ("POST", "/obs/replay/save"),
    ("POST", "/obs/brb"),
    ("GET", "/obs/stream"),
    ("POST", "/obs/stream"),
    ("GET", "/obs/health"),
    ("POST", "/obs/health"),
    ("GET", "/music"),
    ("PUT", "/music"),
    ("GET", "/moments/config"),
    ("PUT", "/moments/config"),
    ("GET", "/moments/lane"),
    ("POST", "/moments/lane/dismiss-all"),
    ("POST", "/moments/lane/*/dismiss"),
    ("GET", "/moments/markers"),
    ("POST", "/moments/markers"),
    ("GET", "/moments/markers/export"),
    ("DELETE", "/moments/markers/*"),
    ("GET", "/automations"),
    ("PUT", "/automations"),
    ("POST", "/automations/*/run"),
    ("GET", "/show"),
    ("GET", "/show/preflight"),
    ("GET", "/show/alerts"),
    ("POST", "/show/live-scene"),
    ("POST", "/show/countdown/*"),
    ("POST", "/show/preflight/fix"),
];

pub const GO_LIVE_ALLOWED: &[Rule] = &[
    ("GET", "/live"),
    ("GET", "/golive"),
    ("PUT", "/golive/destinations"),
    ("POST", "/golive/destinations/*/test"),
    ("PUT", "/golive/destinations/*/light"),
    ("PUT", "/golive/destinations/*/enhanced"),
    ("POST", "/golive/obs/setup"),
    ("POST", "/golive/obs/restore"),
    ("POST", "/golive/upload-check"),
    ("PUT", "/golive/protection"),
    ("PUT", "/golive/aitum"),
    ("PUT", "/golive/auto-quality"),
    ("POST", "/golive/vertical/obs/setup"),
    ("POST", "/golive/vertical/obs/restore"),
    ("PUT", "/golive/vertical/source"),
    ("PUT", "/golive/vertical/cut"),
    ("GET", "/golive/picture"),
    ("POST", "/golive/ffmpeg"),
    ("PUT", "/golive/advanced"),
    ("POST", "/golive/resume/*"),
    ("GET", "/stream-info"),
    ("POST", "/stream-info/apply"),
    ("GET", "/stream-info/categories"),
    ("GET", "/stream-info/categories/as"),
    ("POST", "/stream-info/presets"),
    ("PUT", "/stream-info/presets/*"),
    ("DELETE", "/stream-info/presets/*"),
    ("GET", "/show"),
    ("GET", "/show/config"),
    ("PUT", "/show/config"),
    ("GET", "/show/preflight"),
    ("GET", "/show/alerts"),
    ("POST", "/show/preflight/fix"),
    ("POST", "/show/start"),
    ("POST", "/show/countdown/*"),
    ("POST", "/show/live-scene"),
    ("POST", "/show/end"),
    ("GET", "/obs/stream"),
    ("POST", "/obs/stream"),
    ("GET", "/obs/health"),
    ("POST", "/obs/health"),
];

pub const SETTINGS_ALLOWED: &[Rule] = &[
    ("GET", "/settings"),
    ("PUT", "/settings"),
    ("GET", "/themes"),
    ("GET", "/updates"),
    ("POST", "/setup/complete"),
    ("GET", "/backups"),
    ("POST", "/backups"),
    ("GET", "/obs/switcher"),
    ("PUT", "/obs/switcher"),
];

fn scope_rules(scope: ferrocast_core::api_keys::ApiKeyScope) -> &'static [Rule] {
    use ferrocast_core::api_keys::ApiKeyScope;
    match scope {
        ApiKeyScope::ReadOnly => READ_ONLY_ALLOWED,
        ApiKeyScope::Chat => CHAT_ALLOWED,
        ApiKeyScope::Alerts => ALERTS_ALLOWED,
        ApiKeyScope::StudioControl => STUDIO_CONTROL_ALLOWED,
        ApiKeyScope::GoLive => GO_LIVE_ALLOWED,
        ApiKeyScope::Settings => SETTINGS_ALLOWED,
    }
}

/// Whether an API key holding `scopes` may call `method path` (relative to `/api/v1`, no query).
/// Uses the same rule/segment matcher as remote device roles and enforces [`KEY_NEVER_ALLOWED`].
pub fn api_key_allowed(scopes: &[ferrocast_core::api_keys::ApiKeyScope], method: &str, path: &str) -> bool {
    if scopes.is_empty() {
        return false;
    }
    let Some(segs) = segments(path) else { return false };
    if !matches!(method, "GET" | "POST" | "PUT" | "DELETE") {
        return false;
    }
    if KEY_NEVER_ALLOWED.iter().any(|(m, p)| method_is(m, method) && matches(p, &segs)) {
        return false;
    }
    if KEY_COMMON_READ.iter().any(|(m, p)| method_is(m, method) && matches(p, &segs)) {
        return true;
    }
    scopes.iter().any(|s| scope_rules(*s).iter().any(|(m, p)| method_is(m, method) && matches(p, &segs)))
}

/// Whether a live WebSocket frame may be delivered to an API key with `scopes`.
pub fn api_key_live_frame_allowed(scopes: &[ferrocast_core::api_keys::ApiKeyScope], frame: &str) -> bool {
    use ferrocast_core::api_keys::ApiKeyScope;
    if scopes.is_empty() || !scopes.iter().any(|s| *s != ApiKeyScope::Settings) {
        return false;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(frame) else { return false };
    let ty = v["type"].as_str().unwrap_or("");
    if ty == "account" {
        return false;
    }
    let has = |s: ApiKeyScope| scopes.contains(&s);
    match ty {
        "hello" | "resync" | "status" => true,
        "event" => {
            let kind = v.pointer("/data/kind/type").and_then(|t| t.as_str()).unwrap_or("");
            if has(ApiKeyScope::ReadOnly) {
                return true;
            }
            if MOD_EVENT_KINDS.contains(&kind) { has(ApiKeyScope::Chat) } else { has(ApiKeyScope::Alerts) }
        }
        "chat_highlight" | "held" | "held_resolved" | "panic" | "chatter" | "poll" | "giveaway" | "smart_mod" => {
            has(ApiKeyScope::ReadOnly) || has(ApiKeyScope::Chat)
        }
        "tts_pending" => has(ApiKeyScope::ReadOnly) || has(ApiKeyScope::Chat) || has(ApiKeyScope::Alerts),
        "queue" | "goals" => has(ApiKeyScope::ReadOnly) || has(ApiKeyScope::Alerts),
        "golive" => has(ApiKeyScope::ReadOnly) || has(ApiKeyScope::GoLive) || has(ApiKeyScope::StudioControl),
        "brb" | "obs_health" | "live_alert" | "lane" | "marker" | "show" => {
            has(ApiKeyScope::ReadOnly) || has(ApiKeyScope::StudioControl) || has(ApiKeyScope::GoLive)
        }
        "counter" => has(ApiKeyScope::ReadOnly) || has(ApiKeyScope::Chat) || has(ApiKeyScope::StudioControl),
        "notice" => {
            let key = v.pointer("/data/key").and_then(|k| k.as_str()).unwrap_or("");
            if key.starts_with("account:") {
                false
            } else if key == "panic" {
                has(ApiKeyScope::ReadOnly) || has(ApiKeyScope::Chat)
            } else {
                has(ApiKeyScope::ReadOnly)
                    || has(ApiKeyScope::Alerts)
                    || has(ApiKeyScope::StudioControl)
                    || has(ApiKeyScope::GoLive)
            }
        }
        _ => has(ApiKeyScope::ReadOnly) || has(ApiKeyScope::StudioControl),
    }
}

/// Normalizes a relative API path for redacted audit logs (never keeps query strings or rejected paths).
#[must_use]
pub fn audit_path(path: &str) -> String {
    let clean = path.split('?').next().unwrap_or(path);
    let Some(segs) = segments(clean) else {
        return "/(refused)".to_string();
    };
    let normalized: Vec<&str> = segs
        .iter()
        .map(|s| {
            let plain = s.len() <= 32 && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'-' || b == b'.');
            if plain { *s } else { "{id}" }
        })
        .collect();
    format!("/{}", normalized.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mods_get_only_their_list() {
        for (m, p) in [
            ("GET", "/chat"),
            ("POST", "/mod/ban"),
            ("POST", "/mod/timeout"),
            ("POST", "/mod/delete"),
            ("POST", "/mod/unban"),
            ("POST", "/mod/held/ev_1/release"),
            ("POST", "/mod/held/ev_1/drop"),
            ("POST", "/mod/panic"),
            ("GET", "/mod/panic"),
            ("GET", "/viewers/twitch/123"),
            ("GET", "/status"),
        ] {
            assert!(allowed(Role::Mod, m, p), "{m} {p}");
        }
        for (m, p) in [
            ("POST", "/chat/send"),
            ("GET", "/chat/highlight"),
            ("POST", "/chat/highlight"),
            ("DELETE", "/chat/highlight"),
            ("GET", "/credits"),
            ("GET", "/settings"),
            ("PUT", "/mod/filters"),
            ("PUT", "/mod/panic/config"),
            ("PUT", "/viewers/twitch/123/note"),
            ("GET", "/accounts"),
            ("GET", "/pro"),
            ("GET", "/pro/history/email"),
            ("GET", "/pro/bot"),
            ("PUT", "/pro/bot/enabled"),
            ("DELETE", "/mod/ban"),
            ("GET", "/mod/held/ev_1/drop"),
            ("POST", "/mod/held/a/b/drop"),
            ("GET", "/events"),
            ("GET", "/live"),
            ("GET", "/diagnostics"),
            ("GET", "/diagnostics/preview"),
            ("POST", "/diagnostics/report"),
            ("POST", "/diagnostics/report.zip"),
        ] {
            assert!(!allowed(Role::Mod, m, p), "{m} {p}");
        }
    }

    #[test]
    fn owners_get_everything_but_the_denied_list() {
        for (m, p) in [
            ("GET", "/golive"),
            ("PUT", "/golive/destinations"),
            ("POST", "/golive/upload-check"),
            ("POST", "/golive/obs/setup"),
            ("POST", "/golive/resume/kick"),
            ("POST", "/golive/destinations/c_1/test"),
            ("PUT", "/golive/protection"),
            ("PUT", "/golive/aitum"),
            ("PUT", "/golive/auto-quality"),
            ("GET", "/settings"),
            ("PUT", "/settings"),
            ("POST", "/chat/send"),
            ("GET", "/tips"),
            ("PUT", "/tips/brand"),
            ("GET", "/tips/goal"),
            ("POST", "/test"),
            ("GET", "/obs"),
            ("POST", "/obs/scene"),
            ("GET", "/media/m1"),
            ("DELETE", "/media/m1"),
            ("GET", "/backups"),
            ("POST", "/backups"),
            ("GET", "/updates"),
            ("PUT", "/designs/d1"),
            ("GET", "/pro/history/email"),
            // The cloud bot: look at it and switch it.
            ("GET", "/pro/bot"),
            ("PUT", "/pro/bot/enabled"),
        ] {
            assert!(allowed(Role::Owner, m, p), "{m} {p}");
        }
        for (m, p) in [
            ("GET", "/accounts"),
            ("POST", "/accounts/twitch/connect"),
            ("DELETE", "/accounts/kick"),
            ("POST", "/session/pair-code"),
            ("POST", "/session/pair"),
            ("GET", "/pro"),
            ("POST", "/pro/remote/codes"),
            ("DELETE", "/pro/remote/devices/d1"),
            ("GET", "/pro/backups/key"),
            ("PUT", "/pro/history/email"),
            ("DELETE", "/pro/history/email"),
            ("POST", "/pro/history/email"),
            ("GET", "/pro/history/email/x"),
            // The cloud bot's sign-ins and settings stay on the PC.
            ("POST", "/pro/bot/twitch/connect"),
            ("POST", "/pro/bot/kick/connect"),
            ("DELETE", "/pro/bot/kick"),
            ("DELETE", "/pro/bot/twitch"),
            ("PUT", "/pro/bot/config"),
            ("PUT", "/pro/bot"),
            ("GET", "/pro/bot/enabled"),
            ("PUT", "/tips"),
            ("DELETE", "/tips"),
            ("PUT", "/tips/brand/avatar"),
            ("GET", "/discord"),
            ("PUT", "/discord"),
            ("POST", "/discord/test"),
            ("PUT", "/obs"),
            ("POST", "/obs/detect"),
            // Plugins bring code: never from a phone, not even the list.
            ("GET", "/plugins"),
            ("POST", "/plugins"),
            ("PUT", "/plugins/com.ana.invert"),
            ("DELETE", "/plugins/com.ana.invert"),
            ("POST", "/export"),
            ("POST", "/import"),
            ("POST", "/import/apply"),
            ("POST", "/designs/import"),
            ("POST", "/designs/d1/export"),
            ("POST", "/backups/b1/restore"),
            ("POST", "/media"),
            ("POST", "/updates/install"),
            ("POST", "/updates/check"),
            ("POST", "/updates/restart"),
            ("GET", "/live"),
            ("PATCH", "/settings"),
            ("GET", "/golive/ingest"),
            ("POST", "/golive/destinations"),
            ("DELETE", "/golive/destinations/c_1"),
            ("POST", "/golive/test"),
            ("GET", "/golive/networks"),
            ("GET", "/studio/engine"),
            ("POST", "/studio/engine/start"),
            ("PUT", "/studio/engine/preview"),
            ("GET", "/studio/engine/kinds/color_source/options"),
            ("POST", "/studio/engine/library/collections/overlay/preview"),
            ("POST", "/studio/engine/library/collections/overlay/import"),
            ("POST", "/studio/engine/library/collections/overlay/undo"),
            ("GET", "/diagnostics"),
            ("GET", "/diagnostics/preview"),
            ("POST", "/diagnostics/report"),
            ("POST", "/diagnostics/report.zip"),
        ] {
            assert!(!allowed(Role::Owner, m, p), "{m} {p}");
        }
    }

    #[test]
    fn odd_paths_are_refused_for_everyone() {
        for p in [
            "",
            "/",
            "mod/ban",
            "/mod//ban",
            "/mod/../accounts",
            "/mod/./ban",
            "/media/%2e%2e",
            "/media/a%2Fb",
            "/media/a%5Cb",
            "/media/a%00",
            "/media/a%zz",
            "/media/a b",
            "/media/a\nb",
            "/media/é",
            "/chat?x=1",
        ] {
            assert!(!allowed(Role::Owner, "GET", p), "{p:?}");
            assert!(!allowed(Role::Mod, "GET", p), "{p:?}");
        }
        assert!(allowed(Role::Owner, "GET", "/counters/deaths%20today"));
    }

    #[test]
    fn mods_see_chat_frames_only() {
        let ev = |k: &str| format!(r#"{{"v":1,"seq":1,"type":"event","data":{{"kind":{{"type":"{k}"}}}}}}"#);
        assert!(live_frame_allowed(Role::Mod, &ev("chat_message")));
        assert!(live_frame_allowed(Role::Mod, &ev("ban")));
        assert!(!live_frame_allowed(Role::Mod, &ev("tip")));
        assert!(!live_frame_allowed(Role::Mod, &ev("follow")));
        assert!(live_frame_allowed(Role::Owner, &ev("tip")));
        assert!(live_frame_allowed(Role::Mod, r#"{"v":1,"type":"panic","data":{}}"#));
        assert!(live_frame_allowed(Role::Mod, r#"{"v":1,"type":"notice","data":{"key":"panic"}}"#));
        assert!(!live_frame_allowed(Role::Mod, r#"{"v":1,"type":"notice","data":{"key":"account:twitch"}}"#));
        assert!(!live_frame_allowed(Role::Mod, r#"{"v":1,"type":"queue","data":{}}"#));
        // The Ferrocast account (an email) never leaves the PC, not even to the owner's phone.
        assert!(!live_frame_allowed(Role::Owner, r#"{"v":1,"type":"account","data":{"state":"signed_in"}}"#));
        assert!(!live_frame_allowed(Role::Mod, r#"{"v":1,"type":"account","data":{}}"#));
        assert!(!live_frame_allowed(Role::Mod, "not json"));
    }
}
