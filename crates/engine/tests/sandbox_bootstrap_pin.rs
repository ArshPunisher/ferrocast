//! Pins the custom-alert sandbox bootstrap so its two copies can't drift apart.
//!
//! The same file lives in the web repo at
//! `ferrocast-web/apps/overlays/src/widgets/alert-sandbox-bootstrap.js` (used for the
//! dashboard's srcdoc frame, with a test pinning the same hash). Change both together, then
//! update the hash in both tests. (It is read from the server crate's copy here; the engine
//! crate carries the test because it already depends on `sha2`.)

use sha2::{Digest, Sha256};

/// Keep identical to `BOOTSTRAP_SHA256` in `apps/overlays/test/alert-sandbox-bootstrap.test.ts`.
const BOOTSTRAP_SHA256: &str = "ebf0db667de5a1f383f8a0e7ed51a5c722ef8abc82202bed2660b3859cea4806";

#[test]
fn the_sandbox_bootstrap_matches_the_pinned_hash() {
    let bytes = include_bytes!("../../server/assets/alert-sandbox-bootstrap.js");
    let hash: String = Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        hash, BOOTSTRAP_SHA256,
        "the sandbox bootstrap changed: update the web copy (alert-sandbox-bootstrap.js) and both pinned hashes"
    );
}
