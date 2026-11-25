//! Cameras (DirectShow, read by FFmpeg) and microphones (WASAPI, through cpal) on Windows.

use std::time::Duration;

use crate::ffsrc::{Live, Nv12Picture, OpenFail, ProbeFail, ffmpeg, stderr_within};
use crate::sound_in::{SoundIn, Which};
use crate::win_logic::{DshowListStatus, DshowOption, dshow_attempts, parse_dshow_options, resolve_dshow_list};
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

/// Monotonic counter bumped when connected devices change (no background monitor on Windows).
pub fn hotplug_epoch() -> u64 {
    0
}

/// A running camera or microphone; dropping it stops.
#[derive(Debug)]
pub struct Capture {
    live: Option<Live>,
    sound: Option<SoundIn>,
}

fn list_dshow_options(dev: &str) -> Result<Vec<DshowOption>, ProbeFail> {
    let ff = ffmpeg().map_err(|e| ProbeFail::Error(e.to_string()))?;
    let args: Vec<String> =
        ["-hide_banner", "-nostdin", "-f", "dshow", "-list_options", "true", "-i", &format!("video={dev}")]
            .into_iter()
            .map(String::from)
            .collect();
    let text = stderr_within(&ff, &args, Duration::from_secs(4))?;
    if text.contains("Could not find video device") {
        return Err(ProbeFail::Missing);
    }
    Ok(parse_dshow_options(&text))
}

fn resolve_dshow_options(res: Result<Vec<DshowOption>, ProbeFail>, dev: &str) -> Result<Vec<DshowOption>, AvError> {
    let status = match res {
        Ok(opts) => DshowListStatus::Ok(opts),
        Err(ProbeFail::Missing) => DshowListStatus::Missing,
        Err(ProbeFail::Timeout) => {
            tracing::warn!(dev, "listing camera options timed out; opening with default formats");
            DshowListStatus::TimedOut
        }
        Err(ProbeFail::Error(_)) => DshowListStatus::Failed,
    };
    resolve_dshow_list(status).map_err(|msg| AvError::Source(msg.into()))
}

impl Capture {
    /// What this capture wants said on the dashboard (nothing here).
    pub fn take_notice(&self) -> Option<crate::notices::Notice> {
        None
    }

    /// A camera by its id (an OBS-style "Name:\\?\usb#…" works too: its name is used).
    /// Queries DirectShow options first so USB2 capture cards pick high-fps MJPEG (e.g.
    /// 1080p60) instead of throttled uncompressed YUY2, falling back through MJPEG + asked,
    /// asked, and device default, and reading NV12 through the pipe.
    pub fn camera(spec: &crate::engine::CameraSpec) -> Result<Self, AvError> {
        let id = spec.id.as_str();
        let dev = if id.starts_with('@') { id.to_string() } else { id.split(":\\\\").next().unwrap_or(id).to_string() };
        let options = resolve_dshow_options(list_dshow_options(&dev), &dev)?;
        let max = spec.size.unwrap_or((1920, 1080));
        let mut last_err = AvError::Source("FFmpeg couldn't open it".into());
        for attempt in dshow_attempts(spec, &options) {
            let mut input: Vec<String> = vec!["-f".into(), "dshow".into(), "-rtbufsize".into(), "256M".into()];
            input.extend(attempt.args);
            input.extend(["-i".into(), format!("video={dev}")]);
            let fps = attempt.fps_hint.or(spec.fps).unwrap_or(60);
            match Live::start_nv12_checked(&input, max, fps, None) {
                Ok(live) => return Ok(Capture { live: Some(live), sound: None }),
                Err(OpenFail::Timeout) => {
                    return Err(AvError::Source("FFmpeg timed out opening the camera".into()));
                }
                Err(OpenFail::Missing(e)) => return Err(e),
                Err(OpenFail::Rejected(e)) => last_err = e,
            }
        }
        Err(last_err)
    }

    /// A microphone by name ("default" or empty: the system's).
    pub fn microphone(id: &str) -> Result<Self, AvError> {
        Ok(Capture { live: None, sound: Some(SoundIn::start(Which::Input(Some(id.to_string())))?) })
    }

    /// The newest picture and when it came (taken), converting NV12 to RGBA when the camera
    /// delivers NV12 (e.g. one-shot picker thumbnails).
    pub fn take_frame(&self) -> Option<(Picture, u64)> {
        let live = self.live.as_ref()?;
        live.take().or_else(|| {
            let (nv12, t) = live.take_nv12()?;
            Some((nv12.to_picture()?, t))
        })
    }

    /// The newest NV12 picture and when it came (taken), for GPU upload without CPU RGBA copy.
    pub fn take_nv12(&self) -> Option<(Nv12Picture, u64)> {
        self.live.as_ref()?.take_nv12()
    }

    pub fn take_sound(&self) -> Vec<(Vec<f32>, u64)> {
        self.sound.as_ref().map(SoundIn::take_sound).unwrap_or_default()
    }

    /// Pictures (a camera) or sound pieces (a microphone) received so far.
    pub fn received(&self) -> u64 {
        self.live.as_ref().map_or(0, Live::frames) + self.sound.as_ref().map_or(0, SoundIn::received)
    }

    /// A camera or a microphone sends all the time: quiet for long means it stopped (a
    /// microphone counted nothing before 20 Nov, so the watchdog never saw one stop).
    pub fn steady(&self) -> bool {
        self.live.is_some() || self.sound.is_some()
    }

    /// Why FFmpeg's camera or the microphone ended, if it did.
    pub fn ended(&self) -> Option<String> {
        self.live.as_ref().and_then(Live::ended).or_else(|| self.sound.as_ref()?.ended())
    }

    /// DirectShow/WASAPI has no separate suspended-interruption state.
    pub fn interrupted(&self) -> bool {
        false
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
