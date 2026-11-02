//! Cameras (DirectShow, read by FFmpeg) and microphones (WASAPI, through cpal) on Windows.

use crate::ffsrc::{Live, ffmpeg};
use crate::sound_in::{SoundIn, Which};
use crate::{AvError, Picture};

/// A camera or microphone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// Stable between runs (what a scene saves): a camera's DirectShow "alternative name",
    /// a microphone's name.
    pub id: String,
    pub name: String,
}

/// DirectShow's video devices from FFmpeg's list: the name, then its alternative name.
pub fn parse_dshow(text: &str) -> Vec<Device> {
    let mut out: Vec<Device> = Vec::new();
    let mut pending: Option<String> = None;
    for line in text.lines() {
        let l = line.split_once(']').map_or(line, |(_, r)| r).trim();
        if let Some(rest) = l.strip_prefix("Alternative name ") {
            if let Some(name) = pending.take() {
                out.push(Device { id: rest.trim_matches('"').to_string(), name });
            }
        } else if l.ends_with("(video)") {
            pending = l.split('"').nth(1).map(String::from);
        } else if l.ends_with("(audio)") {
            pending = None;
        }
    }
    out
}

/// Cameras connected now (FFmpeg asks DirectShow; none without FFmpeg yet).
pub fn cameras() -> Vec<Device> {
    let Ok(ff) = ffmpeg() else { return Vec::new() };
    let mut cmd = std::process::Command::new(ff);
    cmd.args(["-hide_banner", "-list_devices", "true", "-f", "dshow", "-i", "dummy"]);
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd.output().map(|o| parse_dshow(&String::from_utf8_lossy(&o.stderr))).unwrap_or_default()
}

/// Microphones and other inputs connected now.
pub fn microphones() -> Vec<Device> {
    crate::sound_in::inputs().into_iter().map(|n| Device { id: n.clone(), name: n }).collect()
}

/// A running camera or microphone; dropping it stops.
#[derive(Debug)]
pub struct Capture {
    live: Option<Live>,
    sound: Option<SoundIn>,
}

impl Capture {
    /// A camera by its id (an OBS-style "Name:\\?\usb#…" works too: its name is used).
    pub fn camera(id: &str) -> Result<Self, AvError> {
        let dev = if id.starts_with('@') { id.to_string() } else { id.split(":\\\\").next().unwrap_or(id).to_string() };
        let input =
            vec!["-f".into(), "dshow".into(), "-rtbufsize".into(), "256M".into(), "-i".into(), format!("video={dev}")];
        Ok(Capture { live: Some(Live::start(&input, (1920, 1080), 60, None)?), sound: None })
    }

    /// A microphone by name ("default" or empty: the system's).
    pub fn microphone(id: &str) -> Result<Self, AvError> {
        Ok(Capture { live: None, sound: Some(SoundIn::start(Which::Input(Some(id.to_string())))?) })
    }

    /// The newest picture and when it came (taken).
    pub fn take_frame(&self) -> Option<(Picture, u64)> {
        self.live.as_ref()?.take()
    }

    pub fn take_sound(&self) -> Vec<(Vec<f32>, u64)> {
        self.sound.as_ref().map(SoundIn::take_sound).unwrap_or_default()
    }

    /// Pictures received so far.
    pub fn received(&self) -> u64 {
        self.live.as_ref().map_or(0, Live::frames)
    }

    /// A camera sends all the time (a microphone here has no count to watch).
    pub fn steady(&self) -> bool {
        self.live.is_some()
    }

    /// Why FFmpeg's camera ended, if it did.
    pub fn ended(&self) -> Option<String> {
        self.live.as_ref()?.ended()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directshows_list_is_read() {
        let text = r#"[dshow @ 0000020] "Integrated Camera" (video)
[dshow @ 0000020]   Alternative name "@device_pnp_\\?\usb#vid_04f2&pid_b6d9&mi_00#6&1a2b"
[dshow @ 0000020] "OBS Virtual Camera" (video)
[dshow @ 0000020]   Alternative name "@device_sw_{860BB310}\{A3FCE0F5}"
[dshow @ 0000020] "Microphone (Realtek)" (audio)
[dshow @ 0000020]   Alternative name "@device_cm_{33D9A762}\wave_{1B2C}"
dummy: Immediate exit requested"#;
        let d = parse_dshow(text);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].name, "Integrated Camera");
        assert!(d[0].id.starts_with("@device_pnp_"));
        assert_eq!(d[1].name, "OBS Virtual Camera");
    }
}
