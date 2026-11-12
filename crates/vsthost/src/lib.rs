//! VST3 sound plugins for Ferrocast, each run in its own locked-down helper process
//! (`ferrocast-vst`): the helper (`helper`), what Ferrocast and it say (`proto`), and Ferrocast's
//! side (`client`). See docs/plan/VST.md.

pub mod client;
pub mod effect;
pub mod helper;
pub mod proto;

/// A sound effect found in a VST3 bundle.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Found {
    /// The bundle (`….vst3`).
    pub path: String,
    /// Its class id, 32 hex digits.
    pub cid: String,
    pub name: String,
    pub vendor: String,
    /// The plugin's own categories ("Fx|Reverb").
    pub kinds: String,
}

pub use client::{Helper, scan};

/// Whether VST plugins run here: Err says why not (no sandbox for them on this system yet,
/// or no helper in this build).
pub fn available() -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("VST plugins come to this system with their sandbox (Mac only for now).".into());
    }
    let exe = client::helper_exe().ok_or("This build has no VST helper.")?;
    // Locked down, or not at all: once, whether the helper can be put in its sandbox here (it
    // can't when Ferrocast itself runs inside one).
    static LOCKS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*LOCKS.get_or_init(|| client::probe(&exe, "read", "/System/Library/CoreServices/SystemVersion.plist")) {
        return Err(
            "VST plugins can't be locked down here (Ferrocast is running in a sandbox of its own), so they're off."
                .into(),
        );
    }
    Ok(())
}

/// A plugin's state as kept in its filter's settings (`state`).
pub fn encode_state(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

static FOUND: std::sync::Mutex<Option<Vec<Found>>> = std::sync::Mutex::new(None);

/// The plugins on this computer: looked for once, again when `rescan`.
pub fn plugins(rescan: bool) -> Vec<Found> {
    if available().is_err() {
        return Vec::new();
    }
    if !rescan && let Some(l) = FOUND.lock().ok().and_then(|f| f.clone()) {
        return l;
    }
    let l = scan();
    if let Ok(mut f) = FOUND.lock() {
        *f = Some(l.clone());
    }
    l
}
pub use effect::{install, live};
