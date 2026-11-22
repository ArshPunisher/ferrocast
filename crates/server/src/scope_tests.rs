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
