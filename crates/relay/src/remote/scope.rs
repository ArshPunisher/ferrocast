//! What a paired phone may reach through the tunnel, checked here before anything is
//! forwarded and again on the PC (`crates/server/src/routes/remote_scope.rs`, the same rules;
//! a test in the server crate keeps the two copies equal).
//!
//! Paths are the dashboard API's, relative to `/api/v1` (`/mod/ban`). A path that isn't
//! plain (`..`, `//`, an encoded slash or dot, control characters) is refused for every role.
//!
//! - `owner`: every dashboard route except accounts and sign-ins, secrets (tip provider
//!   secrets, the Discord webhook, the OBS password), Pro and billing, export and import,
//!   restoring a backup, installing an update and media uploads.
//! - `mod`: chat (read), ban, timeout, delete, unban, held messages (approve or remove),
//!   panic on and off, and the viewer card.

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
    // Accounts and sign-ins, and pairing more browsers.
    ("*", "/accounts"),
    ("*", "/accounts/**"),
    ("*", "/session/**"),
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
    // the PC. Watching, ticking platforms and the checks are fine from a phone.
    ("*", "/golive/ingest"),
    ("POST", "/golive/destinations"),
    ("*", "/golive/destinations/*"),
];

/// The dashboard API's routes with a variable part, for the audit log's action names:
/// `/mod/held/ev_123/drop` is kept as `/mod/held/{id}/drop`, never with the id.
const TEMPLATES: &[(&str, &str)] = &[
    ("/accounts/*/connect", "/accounts/{platform}/connect"),
    ("/accounts/*", "/accounts/{platform}"),
    ("/overlays/*/regenerate-url", "/overlays/{id}/regenerate-url"),
    ("/overlays/*/add-to-obs", "/overlays/{id}/add-to-obs"),
    ("/overlays/*", "/overlays/{id}"),
    ("/designs/*/duplicate", "/designs/{id}/duplicate"),
    ("/designs/*/use", "/designs/{id}/use"),
    ("/designs/*/export", "/designs/{id}/export"),
    ("/designs/*", "/designs/{id}"),
    ("/alert-rules/*", "/alert-rules/{id}"),
    ("/queue/*/remove", "/queue/{alert_id}/remove"),
    ("/events/*/replay", "/events/{id}/replay"),
    ("/events/*/why", "/events/{id}/why"),
    ("/goals/*/adjust", "/goals/{id}/adjust"),
    ("/goals/*", "/goals/{id}"),
    ("/mod/held/*/release", "/mod/held/{event_id}/release"),
    ("/mod/held/*/drop", "/mod/held/{event_id}/drop"),
    ("/viewers/*/*/note", "/viewers/{platform}/{user_id}/note"),
    ("/viewers/*/*", "/viewers/{platform}/{user_id}"),
    ("/commands/*", "/commands/{id}"),
    ("/timers/*", "/timers/{id}"),
    ("/loyalty/viewers/*/*/adjust", "/loyalty/viewers/{platform}/{user_id}/adjust"),
    ("/loyalty/viewers/*/*", "/loyalty/viewers/{platform}/{user_id}"),
    ("/tts/*/approve", "/tts/{id}/approve"),
    ("/tts/*/reject", "/tts/{id}/reject"),
    ("/media/*", "/media/{id}"),
    ("/tips/brand/*", "/tips/brand/{kind}"),
    ("/backups/*/restore", "/backups/{id}/restore"),
    ("/pro/backups/*/restore", "/pro/backups/{id}/restore"),
    ("/pro/backups/*", "/pro/backups/{id}"),
    ("/counters/*/change", "/counters/{name}/change"),
    ("/counters/*", "/counters/{name}"),
    ("/quotes/*", "/quotes/{id}"),
    ("/stream-info/presets/*", "/stream-info/presets/{id}"),
    ("/moments/lane/*/dismiss", "/moments/lane/{id}/dismiss"),
    ("/moments/markers/*", "/moments/markers/{id}"),
    ("/autoreplies/*", "/autoreplies/{id}"),
];

/// Fixed routes that look like a template above (`/designs/active-alert`, not a design id).
const FIXED: &[&str] = &[
    "/designs/active-alert",
    "/designs/import",
    "/moments/markers/export",
    "/moments/lane/dismiss-all",
    "/autoreplies/order",
    "/autoreplies/try",
    "/autoreplies/starters",
    "/stream-info/presets",
    "/stream-info/categories",
    "/stream-info/apply",
    "/tts/pending",
    "/tts/voices",
    "/tts/cloud",
    "/tts/preview",
    "/mod/bans",
    "/mod/panic",
    "/mod/held",
    // Common changes from a phone, named in the desktop's activity list.
    "/settings",
    "/chat/send",
    "/test",
    "/queue/pause",
    "/queue/resume",
    "/queue/skip",
    "/obs/scene",
    "/mod/ban",
    "/mod/unban",
    "/mod/timeout",
    "/mod/delete",
    "/mod/filters",
    "/mod/panic/config",
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

/// The audit log's name for a request: `POST /mod/held/{event_id}/drop`. Ids, names and
/// anything else that came from the phone never reach the log.
///
/// `known`: the PC answered (anything but 404/405), so the route exists and its plain words
/// may be kept (`POST /goals/reset`). Otherwise only listed routes keep their name and the
/// rest is `METHOD {unknown}`: a phone can't write words of its choosing into the log.
pub fn action(method: &str, path: &str, known: bool) -> String {
    let method = if matches!(method, "GET" | "POST" | "PUT" | "DELETE") { method } else { "OTHER" };
    let Some(segs) = segments(path) else { return format!("{method} (refused path)") };
    if FIXED.contains(&path) {
        return format!("{method} {path}");
    }
    if let Some((_, t)) = TEMPLATES.iter().find(|(p, _)| matches(p, &segs)) {
        return format!("{method} {t}");
    }
    if !known {
        // A refused route an owner may never use keeps the rule's fixed words.
        if let Some((_, rule)) = OWNER_DENIED.iter().find(|(_, p)| matches(p, &segs)) {
            let named: Vec<&str> = rule
                .trim_start_matches('/')
                .split('/')
                .map(|w| if w == "*" || w == "**" { "{…}" } else { w })
                .collect();
            return format!("{method} /{}", named.join("/"));
        }
        return format!("{method} {{unknown}}");
    }
    // A route without variable parts keeps its words; anything else is a variable part.
    let named: Vec<String> = segs
        .iter()
        .map(|s| {
            let word = s.len() <= 40 && s.bytes().all(|c| c.is_ascii_lowercase() || c == b'-');
            if word { (*s).to_string() } else { "{id}".to_string() }
        })
        .collect();
    format!("{method} /{}", named.join("/"))
}

/// Event kinds a mod's live socket gets (`event` frames): chat and what mods do to it.
pub const MOD_EVENT_KINDS: &[&str] = &["chat_message", "message_deleted", "user_messages_cleared", "ban"];

/// Live socket frame types a mod gets; owners get every frame.
pub const MOD_FRAMES: &[&str] = &["hello", "resync", "status", "event", "held", "held_resolved", "panic", "chatter"];

/// Whether a live frame (`{ v, type, data }`, as text) may go to `role`.
pub fn live_frame_allowed(role: Role, frame: &str) -> bool {
    if role == Role::Owner {
        return true;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(frame) else { return false };
    let ty = v["type"].as_str().unwrap_or("");
    match ty {
        "event" => v.pointer("/data/kind/type").and_then(|t| t.as_str()).is_some_and(|k| MOD_EVENT_KINDS.contains(&k)),
        // Only the panic notice; others can name accounts, files or settings.
        "notice" => v.pointer("/data/key").and_then(|k| k.as_str()) == Some("panic"),
        t => MOD_FRAMES.contains(&t),
    }
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
            ("GET", "/settings"),
            ("PUT", "/mod/filters"),
            ("PUT", "/mod/panic/config"),
            ("PUT", "/viewers/twitch/123/note"),
            ("GET", "/accounts"),
            ("GET", "/pro"),
            ("DELETE", "/mod/ban"),
            ("GET", "/mod/held/ev_1/drop"),
            ("POST", "/mod/held/a/b/drop"),
            ("GET", "/events"),
            ("GET", "/live"),
        ] {
            assert!(!allowed(Role::Mod, m, p), "{m} {p}");
        }
    }

    #[test]
    fn owners_get_everything_but_the_denied_list() {
        for (m, p) in [
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
            // Stream history (the relay answers it while the PC is off, M10).
            ("GET", "/pro/history"),
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
            ("GET", "/pro/history/export.csv"),
            ("POST", "/pro/history/streams"),
            ("PUT", "/tips"),
            ("DELETE", "/tips"),
            ("PUT", "/tips/brand/avatar"),
            ("GET", "/discord"),
            ("PUT", "/discord"),
            ("POST", "/discord/test"),
            ("PUT", "/obs"),
            ("POST", "/obs/detect"),
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
    fn audit_actions_never_keep_ids() {
        assert_eq!(action("POST", "/mod/held/ev_9f2/drop", true), "POST /mod/held/{event_id}/drop");
        assert_eq!(action("GET", "/viewers/twitch/123", true), "GET /viewers/{platform}/{user_id}");
        assert_eq!(action("PUT", "/designs/active-alert", true), "PUT /designs/active-alert");
        assert_eq!(action("PUT", "/counters/deaths", true), "PUT /counters/{name}");
        assert_eq!(action("POST", "/mod/ban", false), "POST /mod/ban");
        assert_eq!(action("POST", "/new/Thing42", true), "POST /new/{id}");
        assert_eq!(action("GET", "/a/../b", true), "GET (refused path)");
        assert_eq!(action("PATCH", "/x", true), "OTHER /x");
    }

    #[test]
    fn unknown_routes_are_never_named_by_the_phone() {
        // Words a phone made up (the PC said 404, or the relay refused it) stay out of the log.
        assert_eq!(action("POST", "/call-me-at-555-0100", false), "POST {unknown}");
        assert_eq!(action("PUT", "/hello/there", false), "PUT {unknown}");
        // Listed routes keep their names whatever happened.
        assert_eq!(action("PUT", "/settings", false), "PUT /settings");
        assert_eq!(action("PUT", "/mod/filters", false), "PUT /mod/filters");
        // A refused owner-only route keeps the rule's words, not the phone's.
        assert_eq!(action("POST", "/accounts/twitch/connect", false), "POST /accounts/{platform}/connect");
        assert_eq!(action("GET", "/pro/whatever/i/like", false), "GET /pro/{…}");
        assert_eq!(action("POST", "/export", false), "POST /export");
        // A route the PC answered keeps its plain words.
        assert_eq!(action("POST", "/giveaway/draw", true), "POST /giveaway/draw");
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
        assert!(!live_frame_allowed(Role::Mod, "not json"));
    }
}
