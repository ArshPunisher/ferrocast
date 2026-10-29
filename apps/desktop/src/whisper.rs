//! Live captions with Whisper (plan item 7b, 30 Oct): whisper.cpp on this computer (Metal on a
//! Mac), so captions work on Windows too, in Hindi and many more languages, and can come out in
//! English whatever is said. A worker thread reads the sound every two seconds, writes what it
//! heard so far, and finishes a sentence at a pause or after eight seconds. Silence is skipped:
//! it costs nothing and Whisper invents words in it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferrocast_studio::TapReader;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

/// Whisper hears at 16 kHz.
const RATE: usize = 16_000;
/// How often it looks again while you talk.
const STEP: Duration = Duration::from_secs(2);
/// A sentence is finished after this long at most.
const MAX_SECONDS: usize = 8;
/// Quieter than this (RMS) is silence.
const SILENCE: f32 = 0.004;
/// A segment Whisper thinks is more likely silence than speech is dropped.
const NO_SPEECH: f32 = 0.6;

pub struct Options {
    pub model: PathBuf,
    /// "auto" or a language code ("en", "hi"…).
    pub language: String,
    pub translate: bool,
}

/// Captions running; dropping it stops the worker.
pub struct WhisperListener {
    stop: Arc<AtomicBool>,
    failed: Arc<Mutex<Option<String>>>,
}

impl WhisperListener {
    /// Why it stopped by itself (the model didn't load), if it did.
    pub fn failed(&self) -> Option<String> {
        self.failed.lock().ok().and_then(|f| f.clone())
    }
}

impl Drop for WhisperListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Starts listening to what `reader` hears (mono, `rate` Hz). `on_text` gets the words heard
/// so far in this sentence, and true when it's finished; it's called on the worker thread.
pub fn start(
    reader: TapReader,
    rate: u32,
    opts: Options,
    on_text: impl Fn(String, bool) + Send + 'static,
) -> WhisperListener {
    let stop = Arc::new(AtomicBool::new(false));
    let failed = Arc::new(Mutex::new(None));
    let (stop2, failed2) = (stop.clone(), failed.clone());
    let spawned = std::thread::Builder::new().name("whisper captions".into()).spawn(move || {
        if let Err(e) = run(&reader, rate, &opts, &stop2, &on_text) {
            tracing::warn!(error = %e, "Whisper captions stopped");
            if let Ok(mut f) = failed2.lock() {
                *f = Some(e);
            }
        }
    });
    if let Err(e) = spawned
        && let Ok(mut f) = failed.lock()
    {
        *f = Some(format!("Captions couldn't start: {e}"));
    }
    WhisperListener { stop, failed }
}

fn run(
    reader: &TapReader,
    rate: u32,
    opts: &Options,
    stop: &AtomicBool,
    on_text: &dyn Fn(String, bool),
) -> Result<(), String> {
    whisper_rs::install_logging_hooks();
    let ctx = WhisperContext::new_with_params(&opts.model, WhisperContextParameters::default())
        .map_err(|e| format!("The speech model didn't load ({e}). Remove it and download it again."))?;
    let mut state = ctx.create_state().map_err(|e| format!("The speech model didn't start ({e})."))?;
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get().saturating_sub(2).clamp(1, 4)) as i32;
    let mut window: Vec<f32> = Vec::with_capacity(RATE * MAX_SECONDS);
    let mut last_run = Instant::now();
    let mut heard_any = false;
    while !stop.load(Ordering::Relaxed) && !reader.closed() {
        std::thread::sleep(Duration::from_millis(200));
        window.extend(resample(&reader.take(), rate as usize, RATE));
        if window.len() < RATE {
            continue;
        }
        let full = window.len() >= RATE * MAX_SECONDS;
        // A pause after speech finishes the sentence; silence alone is thrown away.
        let pause = rms(&window[window.len().saturating_sub(RATE * 7 / 10)..]) < SILENCE;
        if !full && !pause && last_run.elapsed() < STEP {
            continue;
        }
        if rms(&window) < SILENCE {
            window.clear();
            continue;
        }
        last_run = Instant::now();
        let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        p.set_n_threads(threads);
        p.set_language(Some(opts.language.as_str()));
        p.set_translate(opts.translate);
        p.set_no_context(true);
        p.set_suppress_blank(true);
        p.set_no_speech_thold(NO_SPEECH);
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_special(false);
        p.set_print_timestamps(false);
        if let Err(e) = state.full(p, &window) {
            tracing::debug!(error = %e, "Whisper didn't read a stretch");
            window.clear();
            continue;
        }
        let said: Vec<String> = state
            .as_iter()
            .filter(|s| s.no_speech_probability() < NO_SPEECH)
            .filter_map(|s| s.to_str_lossy().ok().map(|t| t.into_owned()))
            .collect();
        let text = clean(&said.join(" "));
        let last = full || (pause && heard_any);
        if !text.is_empty() {
            heard_any = true;
            on_text(text, last);
        } else if last {
            on_text(String::new(), true);
        }
        if last || full {
            window.clear();
            heard_any = false;
        }
    }
    Ok(())
}

/// `input` at `from` Hz as `to` Hz (linear; speech needs no better).
pub(crate) fn resample(input: &[f32], from: usize, to: usize) -> Vec<f32> {
    if input.is_empty() || from == 0 || to == 0 {
        return Vec::new();
    }
    if from == to {
        return input.to_vec();
    }
    // 48 kHz to 16 kHz, the usual case: the average of each three.
    if from == to * 3 {
        return input.chunks(3).map(|c| c.iter().sum::<f32>() / c.len() as f32).collect();
    }
    let n = (input.len() as u64 * to as u64 / from as u64) as usize;
    (0..n)
        .map(|i| {
            let pos = i as f64 * from as f64 / to as f64;
            let j = pos as usize;
            let f = (pos - j as f64) as f32;
            let a = input[j.min(input.len() - 1)];
            let b = input[(j + 1).min(input.len() - 1)];
            a + (b - a) * f
        })
        .collect()
}

pub(crate) fn rms(s: &[f32]) -> f32 {
    if s.is_empty() {
        return 0.0;
    }
    (s.iter().map(|v| v * v).sum::<f32>() / s.len() as f32).sqrt()
}

/// Whisper's words without its notes on the sound ("[BLANK_AUDIO]", "(music)", "♪").
pub(crate) fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0i32;
    for c in text.chars() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' => depth = (depth - 1).max(0),
            '♪' | '♫' => {}
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_keeps_time() {
        assert_eq!(resample(&[1.0; 4800], 48_000, 16_000).len(), 1600);
        assert_eq!(resample(&[0.5; 4410], 44_100, 16_000).len(), 1600);
        assert_eq!(resample(&[0.25; 160], 16_000, 16_000), vec![0.25; 160]);
        assert!(resample(&[], 48_000, 16_000).is_empty());
        assert!(resample(&[1.0], 0, 16_000).is_empty());
        // A constant stays constant.
        assert!(resample(&[0.5; 4410], 44_100, 16_000).iter().all(|v| (v - 0.5).abs() < 1e-6));
    }

    #[test]
    fn silence_is_quiet_and_speech_isnt() {
        assert_eq!(rms(&[]), 0.0);
        assert!(rms(&[0.001; 100]) < SILENCE);
        assert!(rms(&[0.2, -0.2, 0.2, -0.2]) > SILENCE);
    }

    #[test]
    fn notes_on_the_sound_are_taken_out() {
        assert_eq!(clean(" [BLANK_AUDIO] "), "");
        assert_eq!(clean("(music) Hello  there ♪"), "Hello there");
        assert_eq!(clean("नमस्ते दोस्तों [laughs] आज"), "नमस्ते दोस्तों आज");
        assert_eq!(clean("clip that!"), "clip that!");
    }
}
