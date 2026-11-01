//! A Studio sound's audio, as our engine mixes it (after its filters), handed to anything that
//! listens (plan item 7b, 30 Oct: Whisper captions on every system). Mono float samples at
//! 48 kHz.

use std::sync::Arc;

/// What a sound tap hears, readable from any thread (empty once the tap is dropped).
#[derive(Clone)]
pub struct TapReader(ferrocast_av::mix::Tap);

impl std::fmt::Debug for TapReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TapReader").finish_non_exhaustive()
    }
}

impl TapReader {
    /// Everything heard since the last take (oldest first).
    pub fn take(&self) -> Vec<f32> {
        self.0.take()
    }

    /// Whether the tap is gone (nothing more will come).
    pub fn closed(&self) -> bool {
        Arc::strong_count(&self.0.0) == 1
    }

    /// A reader of our engine's tap (its owner keeps the tap; dropping that closes this).
    pub(crate) fn own(tap: &ferrocast_av::mix::Tap) -> Self {
        Self(tap.clone())
    }
}
