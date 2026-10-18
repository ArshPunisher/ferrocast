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
        let Ok(out) =
            Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", script]).output().await
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

/// Speaks `text` into a WAV file at `out`.
pub async fn synthesize(text: &str, voice: Option<&str>, work: &Path, out: &Path) -> Result<(), SpeechError> {
    tokio::fs::create_dir_all(work).await.map_err(|e| SpeechError::Failed(e.to_string()))?;
    let input: PathBuf =
        work.join(format!("tts-{}.txt", std::process::id() as u64 * 1_000_000 + u64::from(rand_suffix())));
    tokio::fs::write(&input, text.as_bytes()).await.map_err(|e| SpeechError::Failed(e.to_string()))?;
    let result = run(&input, voice.and_then(safe_voice), out).await;
    let _ = tokio::fs::remove_file(&input).await;
    result?;
    let head = tokio::fs::read(out).await.map_err(|e| SpeechError::Failed(e.to_string()))?;
    if !(head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WAVE")) {
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
    let mut c = Command::new("powershell");
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
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("a.wav");
        // Looks like options and shell, must just be read aloud.
        synthesize("-o /tmp/pwned $(say hacked) `id`; hello", None, dir.path(), &out).await.unwrap();
        let bytes = std::fs::read(&out).unwrap();
        assert!(bytes.starts_with(b"RIFF") && bytes.len() > 1_000);
        assert!(!std::path::Path::new("/tmp/pwned").exists());
        assert!(!voices().await.is_empty());
    }
}
