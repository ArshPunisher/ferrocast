//! What each API key scope reaches: no key and no ingest target for go_live, and one exact
//! allowed set per scope (a new route is denied until someone adds it on purpose).

use crate::routes::remote_scope::api_key_allowed;
use ferrocast_core::api_keys::ApiKeyScope;

#[test]
fn go_live_never_reads_a_stream_key_or_sets_an_ingest_target() {
    // Routes that return a key (masked or not), or point a stream somewhere else, or run a
    // download: the owner's PC dashboard only.
    for (m, p) in [
        ("GET", "/golive/ingest"),
        ("POST", "/golive/new-key"),
        ("PUT", "/golive/advanced"),
        ("POST", "/golive/obs/setup"),
        ("POST", "/golive/obs/restore"),
        ("POST", "/golive/vertical/obs/setup"),
        ("POST", "/golive/vertical/obs/restore"),
        ("POST", "/golive/ffmpeg"),
        ("POST", "/golive/destinations"),
        ("DELETE", "/golive/destinations/c_1"),
        ("POST", "/golive/test"),
        ("PUT", "/obs"),
        ("PUT", "/studio/engine/settings"),
        ("GET", "/studio/engine/settings"),
    ] {
        for s in ApiKeyScope::ALL {
            assert!(!api_key_allowed(&[s], m, p), "{s:?} reaches {m} {p}");
        }
    }
}

use crate::routes::remote_scope::{
    ALERTS_ALLOWED, CHAT_ALLOWED, GO_LIVE_ALLOWED, READ_ONLY_ALLOWED, SETTINGS_ALLOWED, STUDIO_CONTROL_ALLOWED,
};
use crate::spec::all_rest_routes;

/// The route list of a scope: every router route (from the spec generator) the scope reaches, as
/// `METHOD /path` with placeholders kept. A new route isn't in it until a rule names it.
fn reached(scope: ApiKeyScope) -> String {
    let mut rows: Vec<String> = all_rest_routes()
        .filter(|r| {
            let probe: String = r
                .rel_path
                .split('/')
                .map(|s| if s.starts_with('{') { "sample" } else { s })
                .collect::<Vec<_>>()
                .join("/");
            api_key_allowed(&[scope], r.method, &probe)
        })
        .map(|r| format!("{} {}", r.method, r.rel_path))
        .collect();
    rows.sort();
    rows.dedup();
    rows.join("\n") + "\n"
}

/// One snapshot per scope: the exact set of routes it reaches. Changing a scope means changing
/// its snapshot on purpose (`BLESS_SCOPES=1 cargo test -p ferrocast-server scope_snapshots`).
#[test]
fn every_scope_reaches_exactly_its_snapshot() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/scope_snapshots");
    for scope in ApiKeyScope::ALL {
        let path = dir.join(format!("{}.txt", scope.as_str()));
        let now = reached(scope);
        if std::env::var_os("BLESS_SCOPES").is_some() {
            std::fs::write(&path, &now).unwrap();
            continue;
        }
        let want = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(now, want, "{scope:?} reaches a different set of routes than {path:?}");
    }
}

#[test]
fn no_scope_uses_a_wildcard_for_a_run_of_segments() {
    for (name, rules) in [
        ("read_only", READ_ONLY_ALLOWED),
        ("chat", CHAT_ALLOWED),
        ("alerts", ALERTS_ALLOWED),
        ("studio_control", STUDIO_CONTROL_ALLOWED),
        ("go_live", GO_LIVE_ALLOWED),
        ("settings", SETTINGS_ALLOWED),
    ] {
        for (m, p) in rules {
            assert!(!p.contains("**"), "{name}: {m} {p} grants a run of segments");
            assert_ne!(*m, "*", "{name}: {p} grants every method");
        }
    }
}

/// Scopes don't reach into each other: control of the stream, chat, key-bearing and PC-only
/// routes each belong to the one scope that is for them.
#[test]
fn scopes_do_not_escape_into_each_other() {
    let only = |scope: ApiKeyScope, m: &str, p: &str| {
        for s in ApiKeyScope::ALL {
            assert_eq!(api_key_allowed(&[s], m, p), s == scope, "{s:?} on {m} {p}");
        }
    };
    // Starting the stream is go_live's, whichever route does it.
    only(ApiKeyScope::GoLive, "POST", "/studio/engine/stream");
    only(ApiKeyScope::GoLive, "POST", "/show/start");
    // Posting to chat: chat's; automations (their steps post to chat) are nobody's to write.
    only(ApiKeyScope::Chat, "POST", "/chat/send");
    for s in ApiKeyScope::ALL {
        assert!(!api_key_allowed(&[s], "PUT", "/automations"), "{s:?} writes automations");
        assert!(!api_key_allowed(&[s], "POST", "/automations/a1/run"), "{s:?} runs automations");
    }
    // Putting an overlay into OBS touches OBS: studio_control, not alerts.
    only(ApiKeyScope::StudioControl, "POST", "/overlays/o1/add-to-obs");
    // The Studio's own settings, files, screen pictures and elevation prompts: PC only.
    for (m, p) in [
        ("PUT", "/studio/engine/settings"),
        ("POST", "/studio/engine/virtualcam/setup"),
        ("GET", "/studio/engine/screens/s1/still"),
        ("GET", "/studio/engine/still"),
        ("GET", "/studio/engine/apps"),
        ("GET", "/studio/engine/sources/x/settings"),
        ("PUT", "/studio/engine/sources/x/settings"),
        ("GET", "/studio/engine/sources/x/picture"),
        ("POST", "/studio/engine/ndi"),
        ("POST", "/studio/engine/projectors"),
        ("DELETE", "/studio/engine/library/collections/c1"),
        ("POST", "/studio/engine/stop"),
        ("DELETE", "/media/m1"),
        ("GET", "/overlays"),
    ] {
        if (m, p) == ("GET", "/overlays") {
            // Overlay tokens: alerts manage overlays, nobody else reads them.
            only(ApiKeyScope::Alerts, m, p);
            continue;
        }
        for s in ApiKeyScope::ALL {
            assert!(!api_key_allowed(&[s], m, p), "{s:?} reaches {m} {p}");
        }
    }
}
