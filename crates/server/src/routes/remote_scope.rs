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
            ("GET", "/pro/history/email"),
            ("GET", "/pro/bot"),
            ("PUT", "/pro/bot/enabled"),
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
            ("GET", "/golive"),
            ("PUT", "/golive/destinations"),
            ("POST", "/golive/upload-check"),
            ("POST", "/golive/obs/setup"),
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
