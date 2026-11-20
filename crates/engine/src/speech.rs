//! Text-to-speech with the voices the computer already has (no downloads, no cloud):
//! `say` on macOS, System.Speech on Windows, espeak-ng or pico2wave on Linux. With Pro, also
//! Ferrocast voices (Google Cloud TTS through the relay, API.md §9.4): see [`CloudTts`].
//!
//! The text always goes through a file, never a shell or an argument, so a viewer's message
//! can't become a command-line option or a script. The WAV lands in the media library and
//! plays inside the overlay, so OBS captures it with the alert (R-38).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use tokio::process::Command;

/// PowerShell with no console window of its own (`ferrocore::ffenc::hidden`): a spoken alert
/// flashed a terminal on screen, on stream, in an installed Ferrocast.
#[cfg(target_os = "windows")]
fn hidden_powershell() -> Command {
    Command::from(ferrocore::ffenc::hidden(std::process::Command::new("powershell")))
}

mod cloud;
#[cfg(test)]
mod cloud_tests;
pub(crate) use cloud::CloudTts;
pub use cloud::{
    CLOUD_MAX_CHARS, CLOUD_PREFIX, CLOUD_TIMEOUT, CloudUsage, CloudView, CloudVoice, NOTICE_KEY, Spoken, is_cloud,
    sample_line,
};

/// The longest a synthesis may take before we give up and the overlay falls back to the
/// browser's own voice.
const SYNTH_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Voice {
    pub id: String,
    pub name: String,
    pub language: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SpeechError {
    #[error("this computer has no text-to-speech voices Ferrocast can use{0}")]
    NoEngine(&'static str),
    #[error("text-to-speech failed: {0}")]
    Failed(String),
}

/// Voice names are passed as an argument, so they're limited to safe characters.
fn safe_voice(v: &str) -> Option<&str> {
    (!v.is_empty()
        && v.len() <= 80
        && v.chars().all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '(' | ')' | '.')))
    .then_some(v)
}

/// Lists installed voices.
pub async fn voices() -> Vec<Voice> {
    #[cfg(target_os = "macos")]
    {
        let Ok(out) = Command::new("say").arg("-v").arg("?").output().await else { return Vec::new() };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let (head, _) = l.split_once('#')?;
                let mut parts: Vec<&str> = head.split_whitespace().collect();
                let lang = parts.pop()?.to_string();
                let name = parts.join(" ");
                (!name.is_empty()).then(|| Voice { id: name.clone(), name, language: lang })
            })
            .collect()
    }
    #[cfg(target_os = "windows")]
    {
        let script = "Add-Type -AssemblyName System.Speech; (New-Object System.Speech.Synthesis.SpeechSynthesizer).GetInstalledVoices() | ForEach-Object { $_.VoiceInfo.Name + '|' + $_.VoiceInfo.Culture.Name }";
        let Ok(out) = hidden_powershell().args(["-NoProfile", "-NonInteractive", "-Command", script]).output().await
        else {
            return Vec::new();
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                l.split_once('|').map(|(n, c)| Voice {
                    id: n.trim().into(),
                    name: n.trim().into(),
                    language: c.trim().into(),
                })
            })
            .collect()
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let Ok(out) = Command::new("espeak-ng").arg("--voices").output().await else { return Vec::new() };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .skip(1)
            .filter_map(|l| {
                let cols: Vec<&str> = l.split_whitespace().collect();
                Some(Voice {
                    id: cols.get(4)?.to_string(),
                    name: cols.get(3)?.to_string(),
                    language: cols.get(1)?.to_string(),
                })
            })
            .collect()
    }
}

/// Lines waiting for the voice, at most; one more is let go (the overlay reads it with the
/// browser's voice). A raid or a gift-sub bomb would else start a speech process per alert.
const MAX_QUEUED: usize = 16;

/// One line is spoken at a time: each is a `say` or a PowerShell with the .NET runtime (60-100 MB
/// and a core on Windows).
static GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
/// Lines in the gate's line, the one being spoken included.
static LINE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static ACTIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static PEAK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Takes a place in line, or says there's no room.
struct Place;

impl Place {
    fn take() -> Option<Self> {
        let before = LINE.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        if before >= MAX_QUEUED {
            LINE.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            return None;
        }
        Some(Place)
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        LINE.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// A name no other line's input file shares: the process, a counter and the clock (the clock
/// alone repeats on Windows, ~15 ms granularity, and two lines deleted each other's input).
fn input_name() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("tts-{}-{seq}-{}.txt", std::process::id(), rand_suffix())
}

/// Speaks `text` into a WAV file at `out`. One at a time, in order; with a long line of them
/// waiting, the newest are let go (`Err`).
pub async fn synthesize(text: &str, voice: Option<&str>, work: &Path, out: &Path) -> Result<(), SpeechError> {
    let Some(_place) = Place::take() else {
        return Err(SpeechError::Failed("too many lines are waiting to be read".into()));
    };
    let _turn = GATE.acquire().await.map_err(|_| SpeechError::Failed("the voice is closed".into()))?;
    synthesize_one(text, voice, work, out).await
}

async fn synthesize_one(text: &str, voice: Option<&str>, work: &Path, out: &Path) -> Result<(), SpeechError> {
    #[cfg(test)]
    {
        let now = ACTIVE.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        PEAK.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
    }
    let result = synthesize_inner(text, voice, work, out).await;
    #[cfg(test)]
    ACTIVE.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    result
}

async fn synthesize_inner(text: &str, voice: Option<&str>, work: &Path, out: &Path) -> Result<(), SpeechError> {
    tokio::fs::create_dir_all(work).await.map_err(|e| SpeechError::Failed(e.to_string()))?;
    let input: PathBuf = work.join(input_name());
    tokio::fs::write(&input, text.as_bytes()).await.map_err(|e| SpeechError::Failed(e.to_string()))?;
    let result = run(&input, voice.and_then(safe_voice), out).await;
    let _ = tokio::fs::remove_file(&input).await;
    result?;
    // Only the first 12 bytes tell a WAV: not the whole file read into memory.
    let mut head = [0u8; 12];
    let ok = match tokio::fs::File::open(out).await {
        Ok(mut f) => tokio::io::AsyncReadExt::read_exact(&mut f, &mut head).await.is_ok(),
        Err(e) => return Err(SpeechError::Failed(e.to_string())),
    };
    if !(ok && head.starts_with(b"RIFF") && &head[8..12] == b"WAVE") {
        let _ = tokio::fs::remove_file(out).await;
        return Err(SpeechError::Failed("the voice didn't produce a WAV file".into()));
    }
    Ok(())
}

fn rand_suffix() -> u32 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0)
}

async fn run(input: &Path, voice: Option<&str>, out: &Path) -> Result<(), SpeechError> {
    let mut cmd = command(input, voice, out)?;
    cmd.kill_on_drop(true);
    let status = tokio::time::timeout(SYNTH_TIMEOUT, cmd.status())
        .await
        .map_err(|_| SpeechError::Failed("took too long".into()))?
        .map_err(|e| SpeechError::Failed(e.to_string()))?;
    if status.success() { Ok(()) } else { Err(SpeechError::Failed(format!("exit status {status}"))) }
}

#[cfg(target_os = "macos")]
fn command(input: &Path, voice: Option<&str>, out: &Path) -> Result<Command, SpeechError> {
    let mut c = Command::new("say");
    if let Some(v) = voice {
        c.arg("-v").arg(v);
    }
    c.arg("-f").arg(input).arg("--file-format=WAVE").arg("--data-format=LEI16@22050").arg("-o").arg(out);
    Ok(c)
}

#[cfg(target_os = "windows")]
fn command(input: &Path, voice: Option<&str>, out: &Path) -> Result<Command, SpeechError> {
    // Paths and the voice reach PowerShell as environment variables, not as script text.
    let script = "Add-Type -AssemblyName System.Speech; $s = New-Object System.Speech.Synthesis.SpeechSynthesizer; \
        if ($env:FC_VOICE) { $s.SelectVoice($env:FC_VOICE) }; $s.SetOutputToWaveFile($env:FC_OUT); \
        $s.Speak([System.IO.File]::ReadAllText($env:FC_IN)); $s.Dispose()";
    let mut c = hidden_powershell();
    c.args(["-NoProfile", "-NonInteractive", "-Command", script]).env("FC_IN", input).env("FC_OUT", out);
    if let Some(v) = voice {
        c.env("FC_VOICE", v);
    }
    Ok(c)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn command(input: &Path, voice: Option<&str>, out: &Path) -> Result<Command, SpeechError> {
    let has =
        |bin: &str| std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).exists()));
    if has("espeak-ng") {
        let mut c = Command::new("espeak-ng");
        if let Some(v) = voice {
            c.arg("-v").arg(v);
        }
        c.arg("-f").arg(input).arg("-w").arg(out);
        Ok(c)
    } else {
        Err(SpeechError::NoEngine(" (install espeak-ng from your package manager)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_names_cant_smuggle_options() {
        assert_eq!(safe_voice("Samantha"), Some("Samantha"));
        assert_eq!(safe_voice("Microsoft Zira Desktop"), Some("Microsoft Zira Desktop"));
        for bad in ["", "-o /etc/passwd", "a;rm -rf", "x`y`", "$(boom)", &"v".repeat(81)] {
            assert_eq!(safe_voice(bad), None, "{bad}");
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn speaks_hostile_text_into_a_real_wav() {
        let _turn = PEAK_TEST.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("a.wav");
        // Looks like options and shell, must just be read aloud.
        synthesize("-o /tmp/pwned $(say hacked) `id`; hello", None, dir.path(), &out).await.unwrap();
        let bytes = std::fs::read(&out).unwrap();
        assert!(bytes.starts_with(b"RIFF") && bytes.len() > 1_000);
        assert!(!std::path::Path::new("/tmp/pwned").exists());
        assert!(!voices().await.is_empty());
    }

    /// The tests that read the voice's concurrency peak take turns.
    static PEAK_TEST: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn input_names_never_repeat_even_across_threads() {
        let all: Vec<String> = (0..8)
            .map(|_| std::thread::spawn(|| (0..2_000).map(|_| input_name()).collect::<Vec<_>>()))
            .collect::<Vec<_>>()
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        let unique: std::collections::HashSet<_> = all.iter().collect();
        assert_eq!(unique.len(), all.len());
    }

    /// A raid's worth of lines at once: one voice at a time, a long line of them let go (agy
    /// GAPS #11).
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn an_alert_flood_speaks_one_at_a_time_and_lets_a_long_line_go() {
        let _turn = PEAK_TEST.lock().await;
        let dir = tempfile::tempdir().unwrap();
        // The old behaviour (no gate, every line at once): several voices at the same time.
        PEAK.store(0, std::sync::atomic::Ordering::SeqCst);
        let ungated: Vec<_> = (0..6)
            .map(|i| {
                let (work, out) = (dir.path().to_path_buf(), dir.path().join(format!("old{i}.wav")));
                tokio::spawn(async move { synthesize_one("hi", None, &work, &out).await })
            })
            .collect();
        for t in ungated {
            t.await.unwrap().unwrap();
        }
        assert!(PEAK.load(std::sync::atomic::Ordering::SeqCst) > 1, "without the gate, voices overlap");
        PEAK.store(0, std::sync::atomic::Ordering::SeqCst);
        let work = dir.path().to_path_buf();
        let tasks: Vec<_> = (0..40)
            .map(|i| {
                let (work, out) = (work.clone(), dir.path().join(format!("{i}.wav")));
                tokio::spawn(async move { synthesize("hi", None, &work, &out).await })
            })
            .collect();
        let mut done = 0;
        let mut dropped = 0;
        for t in tasks {
            match t.await.unwrap() {
                Ok(()) => done += 1,
                Err(e) if e.to_string().contains("too many") => dropped += 1,
                Err(e) => panic!("{e}"),
            }
        }
        assert_eq!(PEAK.load(std::sync::atomic::Ordering::SeqCst), 1, "never two voices at once");
        assert!(dropped > 0 && done >= 1 && done + dropped == 40, "done {done}, dropped {dropped}");
        assert!(done <= MAX_QUEUED + 1);
        // No input file is left behind.
        let left = std::fs::read_dir(&work)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "txt"))
            .count();
        assert_eq!(left, 0);
    }
}
