//! A voice provider for tests: the same voice list as Google's, a fixed short Ogg file, and a
//! record of every call so a test can prove what was (or wasn't) sent. It can be told to fail
//! or to be slow.

use std::sync::Mutex;
use std::time::Duration;

use super::provider::{Audio, CloudVoice, SynthFuture, TtsError, TtsProvider};

/// A tiny Ogg file: one page header (`OggS`, version 0, first page) and an Opus head. Enough
/// for every check that sniffs the type; it isn't meant to be played.
pub const FAKE_OGG: &[u8] = b"OggS\x00\x02\x00\x00\x00\x00\x00\x00\x00\x00\x46\x43\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01\x13OpusHead\x01\x01\x38\x01\x80\xbb\x00\x00\x00\x00\x00";

#[derive(Debug, Default)]
pub struct FakeTts {
    calls: Mutex<Vec<(String, String)>>,
    fail: Mutex<Option<TtsError>>,
    delay: Mutex<Option<Duration>>,
}

impl FakeTts {
    pub fn new() -> FakeTts {
        FakeTts::default()
    }

    /// Every `(text, voice)` it was asked to speak, oldest first (failed ones too).
    pub fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Fail every call with `e` from now on (`None`: work again).
    pub fn fail_with(&self, e: Option<TtsError>) {
        *self.fail.lock().unwrap_or_else(|p| p.into_inner()) = e;
    }

    /// Wait this long before answering.
    pub fn slow(&self, d: Option<Duration>) {
        *self.delay.lock().unwrap_or_else(|p| p.into_inner()) = d;
    }
}

impl TtsProvider for FakeTts {
    fn voices(&self) -> Vec<CloudVoice> {
        super::google::voices()
    }

    fn synthesize<'a>(&'a self, text: &'a str, voice: &'a str) -> SynthFuture<'a> {
        Box::pin(async move {
            self.calls.lock().unwrap_or_else(|p| p.into_inner()).push((text.to_string(), voice.to_string()));
            let delay = *self.delay.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(d) = delay {
                tokio::time::sleep(d).await;
            }
            if let Some(e) = self.fail.lock().unwrap_or_else(|p| p.into_inner()).clone() {
                return Err(e);
            }
            Ok(Audio { bytes: FAKE_OGG.to_vec(), mime: "audio/ogg" })
        })
    }
}
