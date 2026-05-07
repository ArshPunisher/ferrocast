//! Finds the built dashboard and overlays from `ferrocast-web` to embed (ARCHITECTURE.md §9).
//! Order: $FERROCAST_WEB_DIST, then ../ferrocast-web/dist next to this repo, then a
//! placeholder page that explains how to build the frontend.
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=FERROCAST_WEB_DIST");
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let sibling = manifest.join("../../../ferrocast-web/dist");
    let chosen = std::env::var("FERROCAST_WEB_DIST")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.join("dashboard/index.html").exists() && p.join("overlays/index.html").exists())
        .or_else(|| {
            (sibling.join("dashboard/index.html").exists() && sibling.join("overlays/index.html").exists())
                .then_some(sibling)
        })
        .unwrap_or_else(|| manifest.join("placeholder"));
    let chosen = chosen.canonicalize().unwrap_or(chosen);
    println!("cargo:rerun-if-changed={}", chosen.display());
    println!("cargo:rustc-env=FERROCAST_WEB_DIR={}", chosen.display());
}
