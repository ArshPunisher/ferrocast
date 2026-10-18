//! What a voice provider is: a fixed list of voices and one call that turns text into audio.
//! `GoogleTts` in production, `FakeTts` in tests (and in the engine's tests).

use std::future::Future;
use std::pin::Pin;

use serde::Serialize;

/// One voice the dashboard can pick, as `/v1/pro/tts/voices` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CloudVoice {
    /// `google:hi-IN-Neural2-A`: the provider, then its own voice name.
    pub id: String,
    /// "Hindi, female": what the picker shows.
    pub name: String,
    /// BCP 47, `hi-IN`.
    pub language: String,
    /// `female` or `male`.
    pub gender: &'static str,
    /// `Neural2`, `WaveNet`: Google's voice family, shown quietly next to the name.
    pub family: &'static str,
}

/// Audio as the provider made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audio {
    pub bytes: Vec<u8>,
    /// Always `audio/ogg` (Opus) today.
    pub mime: &'static str,
}

/// Why a provider couldn't speak.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TtsError {
    /// The provider's own quota or rate limit (Google 429): nothing was made or billed.
    #[error("the voice provider's quota is used up for now")]
    Quota,
    /// Down, slow, misconfigured, or an answer we can't use. The text is never in here.
    #[error("the voice provider is unavailable: {0}")]
    Unavailable(String),
    /// The provider doesn't know that voice (or the text in it).
    #[error("the voice provider doesn't have that voice")]
    BadVoice,
    /// Over the provider's own input limit.
    #[error("the text is too long for the voice provider")]
    TooLong,
}

pub type SynthFuture<'a> = Pin<Box<dyn Future<Output = Result<Audio, TtsError>> + Send + 'a>>;

/// A voice provider. Boxed futures so the relay can hold any of them as `dyn`.
pub trait TtsProvider: Send + Sync + std::fmt::Debug {
    /// The voices offered, in the order the picker shows them.
    fn voices(&self) -> Vec<CloudVoice>;
    /// Speaks `text` (already checked: 1 to 300 characters) in `voice` (one of `voices()`).
    fn synthesize<'a>(&'a self, text: &'a str, voice: &'a str) -> SynthFuture<'a>;
}

/// Ogg pages start with `OggS`; anything else isn't audio we can hand out.
pub fn is_ogg(bytes: &[u8]) -> bool {
    bytes.len() > 27 && bytes.starts_with(b"OggS")
}
