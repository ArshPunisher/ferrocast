//! The song's name: from the media player (Spotify or Music on macOS, the system's media
//! controls on Windows, MPRIS through playerctl on Linux) or from a text file.
//!
//! Each read is a short helper process with a time limit, so a stuck player never stalls the
//! engine. What comes back is cut to one short line each; overlays set it as text only.

use std::path::Path;
use std::time::Duration;

use super::Song;

const LIMIT: Duration = Duration::from_secs(4);
const MAX_FILE: u64 = 4096;
const MAX_TEXT: usize = 200;

/// One line, without control characters, up to 200 letters; `None` when empty.
fn clean(s: &str) -> Option<String> {
    let t: String = s.chars().filter(|c| !c.is_control()).take(MAX_TEXT).collect();
    let t = t.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// `status`, `title`, `artist` on three lines (what each player script prints).
pub(crate) fn parse_player(out: &str) -> Song {
    let mut lines = out.lines();
    let status = lines.next().unwrap_or("").trim().to_lowercase();
    if status != "playing" {
        return Song { playing: Some(false), ..Default::default() };
    }
    Song { title: lines.next().and_then(clean), artist: lines.next().and_then(clean), playing: Some(true) }
}

/// The first line is the song, the second (if any) the artist: never guessed from dashes,
/// since every app writes "Artist - Title" its own way round.
pub(crate) fn parse_file(text: &str) -> Song {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    Song { title: lines.next().and_then(clean), artist: lines.next().and_then(clean), playing: None }
}

pub(crate) async fn from_file(path: &Path) -> Result<Song, String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let f = std::fs::File::open(&path).map_err(|_| {
            "The song file can't be opened. Check the app that writes it is running, or pick the file again."
                .to_string()
        })?;
        let mut buf = Vec::new();
        f.take(MAX_FILE).read_to_end(&mut buf).map_err(|_| "The song file can't be read.".to_string())?;
        Ok(parse_file(&String::from_utf8_lossy(&buf)))
    })
    .await
    .map_err(|_| "The song file can't be read.".to_string())?
}

async fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    {
        // No console window flashing up every few seconds.
        cmd.creation_flags(0x0800_0000);
    }
    match tokio::time::timeout(LIMIT, cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(Ok(_)) => Err(PLAYER_REFUSED.into()),
        Ok(Err(_)) => Err(PLAYER_MISSING.into()),
        Err(_) => Err("The music player didn't answer in time.".into()),
    }
}

#[cfg(target_os = "macos")]
const PLAYER_REFUSED: &str = "macOS didn't let Ferrocast ask Spotify or Music. Open System Settings, Privacy & Security, Automation and allow Ferrocast.";
#[cfg(not(target_os = "macos"))]
const PLAYER_REFUSED: &str = "The music player couldn't say what's playing.";
#[cfg(target_os = "linux")]
const PLAYER_MISSING: &str = "Install playerctl so Ferrocast can see what your music player plays.";
#[cfg(not(target_os = "linux"))]
const PLAYER_MISSING: &str = "The music player couldn't be asked.";

/// Spotify first, then Music; neither is opened if it isn't running.
#[cfg(target_os = "macos")]
const MAC_SCRIPT: &str = r"function run() {
  for (const id of ['Spotify', 'Music']) {
    try {
      const a = Application(id)
      if (!a.running()) continue
      if (String(a.playerState()) !== 'playing') continue
      const t = a.currentTrack()
      return ['playing', t.name(), t.artist()].join('\n')
    } catch (e) {}
  }
  return 'stopped'
}";

#[cfg(windows)]
const WINDOWS_SCRIPT: &str = r#"$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Runtime.WindowsRuntime
$asTask = ([System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object { $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1' })[0]
function Await($op, $type) { $t = $asTask.MakeGenericMethod($type).Invoke($null, @($op)); $t.Wait(3000) | Out-Null; $t.Result }
[Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager, Windows.Media.Control, ContentType = WindowsRuntime] | Out-Null
$m = Await ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager]::RequestAsync()) ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager])
$s = $m.GetCurrentSession()
if (-not $s -or $s.GetPlaybackInfo().PlaybackStatus -ne 'Playing') { 'stopped'; exit 0 }
$p = Await ($s.TryGetMediaPropertiesAsync()) ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionMediaProperties])
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
"playing`n$($p.Title)`n$($p.Artist)""#;

pub(crate) async fn from_player() -> Result<Song, String> {
    #[cfg(target_os = "macos")]
    {
        run("osascript", &["-l", "JavaScript", "-e", MAC_SCRIPT]).await.map(|o| parse_player(&o))
    }
    #[cfg(windows)]
    {
        run(
            "powershell.exe",
            &["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", WINDOWS_SCRIPT],
        )
        .await
        .map(|o| parse_player(&o))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        match run("playerctl", &["metadata", "--format", "{{status}}\n{{title}}\n{{artist}}"]).await {
            Ok(o) => Ok(parse_player(&o)),
            // playerctl exits non-zero when no player is open: nothing plays.
            Err(e) if e == PLAYER_REFUSED => Ok(Song { playing: Some(false), ..Default::default() }),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_answers_are_read_safely() {
        let s = parse_player("playing\nMidnight City\nM83\n");
        assert_eq!(s, Song { title: Some("Midnight City".into()), artist: Some("M83".into()), playing: Some(true) });
        assert_eq!(parse_player("Playing\nA\n").artist, None);
        assert_eq!(parse_player("stopped").playing, Some(false));
        assert_eq!(parse_player("").playing, Some(false));
        let long = format!("playing\n{}\x07bell\nx", "a".repeat(500));
        let t = parse_player(&long).title.unwrap();
        assert_eq!(t.chars().count(), 200);
        assert!(!t.contains('\x07'));
    }

    #[test]
    fn a_file_gives_the_song_then_the_artist() {
        assert_eq!(parse_file("\n  M83 - Midnight City  \n").title.as_deref(), Some("M83 - Midnight City"));
        let s = parse_file("Midnight City\nM83\nignored");
        assert_eq!((s.title.as_deref(), s.artist.as_deref()), (Some("Midnight City"), Some("M83")));
        assert_eq!(parse_file("").title, None);
        assert_eq!(parse_file("x").playing, None, "a file can't say it's paused");
    }

    #[tokio::test]
    async fn files_are_read_up_to_a_small_limit() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("now.txt");
        std::fs::write(&p, format!("Song\nArtist\n{}", "z".repeat(10_000))).unwrap();
        let s = from_file(&p).await.unwrap();
        assert_eq!(s.title.as_deref(), Some("Song"));
        let e = from_file(&dir.path().join("missing.txt")).await.unwrap_err();
        assert!(e.ends_with('.'));
    }
}
