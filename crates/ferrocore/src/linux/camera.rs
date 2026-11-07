//! Cameras (Video4Linux) and microphones (PulseAudio or PipeWire) on Linux, read by FFmpeg.

use std::path::Path;

use crate::ffsrc::{Live, LiveSound};
use crate::{AvError, Picture};

/// A camera or microphone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// Stable between runs (what a scene saves): a camera's `/dev/videoN`, a microphone's
    /// PulseAudio source name.
    pub id: String,
    pub name: String,
}

/// The cameras under `sys` (`/sys/class/video4linux`): each device's main node (index 0; the
/// others are its metadata), by name.
pub fn cameras_in(sys: &Path) -> Vec<Device> {
    let read = |p: std::path::PathBuf| std::fs::read_to_string(p).map(|s| s.trim().to_string()).ok();
    let mut out: Vec<Device> = std::fs::read_dir(sys)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let node = e.file_name().to_string_lossy().into_owned();
            if !node.starts_with("video") || read(e.path().join("index")).is_some_and(|i| i != "0") {
                return None;
            }
            let name = read(e.path().join("name")).unwrap_or_else(|| node.clone());
            Some(Device { id: format!("/dev/{node}"), name })
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Cameras connected now.
pub fn cameras() -> Vec<Device> {
    cameras_in(Path::new("/sys/class/video4linux"))
}

/// PulseAudio's sources from `pactl list short sources` (index, name, driver…), without the
/// speakers' monitors (that's what the computer plays, not a microphone).
pub fn parse_sources(text: &str) -> Vec<Device> {
    text.lines()
        .filter_map(|l| l.split('\t').nth(1))
        .map(str::trim)
        .filter(|n| !n.is_empty() && !n.ends_with(".monitor"))
        .map(|n| Device { id: n.to_string(), name: n.to_string() })
        .collect()
}

/// Microphones and other inputs connected now.
pub fn microphones() -> Vec<Device> {
    parse_sources(&super::output_of("pactl", &["list", "short", "sources"]))
}

/// A running camera or microphone; dropping it stops.
#[derive(Debug)]
pub struct Capture {
    live: Option<Live>,
    sound: Option<LiveSound>,
}

impl Capture {
    /// A camera by its device (`/dev/videoN`).
    pub fn camera(id: &str) -> Result<Self, AvError> {
        if !id.starts_with("/dev/video") {
            return Err(AvError::Source(format!("\"{id}\" isn't a camera")));
        }
        let input = vec!["-f".into(), "v4l2".into(), "-i".into(), id.to_string()];
        Ok(Capture { live: Some(Live::start(&input, (1920, 1080), 60, None)?), sound: None })
    }

    /// A microphone by its PulseAudio name ("default" or empty: the system's).
    pub fn microphone(id: &str) -> Result<Self, AvError> {
        let dev = if id.is_empty() { "default" } else { id };
        let input = vec!["-f".into(), "pulse".into(), "-i".into(), dev.to_string()];
        Ok(Capture { live: None, sound: Some(LiveSound::start(&input)?) })
    }

    /// The newest picture and when it came (taken).
    pub fn take_frame(&self) -> Option<(Picture, u64)> {
        self.live.as_ref()?.take()
    }

    pub fn take_sound(&self) -> Vec<(Vec<f32>, u64)> {
        self.sound.as_ref().map(LiveSound::take_sound).unwrap_or_default()
    }

    /// Pictures received so far.
    pub fn received(&self) -> u64 {
        self.live.as_ref().map_or(0, Live::frames)
    }

    /// A camera sends all the time (a microphone here has no count to watch).
    pub fn steady(&self) -> bool {
        self.live.is_some()
    }

    /// Why FFmpeg ended, if it did.
    pub fn ended(&self) -> Option<String> {
        match (&self.live, &self.sound) {
            (Some(l), _) => l.ended(),
            (None, Some(s)) => s.ended(),
            (None, None) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulse_sources_without_monitors() {
        let text = "0\talsa_output.pci-0000_00_1f.3.analog-stereo.monitor\tPipeWire\ts32le 2ch 48000Hz\tSUSPENDED\n\
                    1\talsa_input.usb-Blue_Yeti-00.analog-stereo\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n";
        let d = parse_sources(text);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].id, "alsa_input.usb-Blue_Yeti-00.analog-stereo");
        assert!(parse_sources("").is_empty());
    }

    #[test]
    fn cameras_are_their_main_nodes() {
        let dir = std::env::temp_dir().join(format!("ferrocore-v4l-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (node, name, index) in
            [("video0", "Logitech C920", "0"), ("video1", "Logitech C920", "1"), ("video2", "OBS Cam", "0")]
        {
            let d = dir.join(node);
            let made = std::fs::create_dir_all(&d)
                .and_then(|()| std::fs::write(d.join("name"), format!("{name}\n")))
                .and_then(|()| std::fs::write(d.join("index"), index));
            assert!(made.is_ok());
        }
        let c = cameras_in(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            c,
            vec![
                Device { id: "/dev/video0".into(), name: "Logitech C920".into() },
                Device { id: "/dev/video2".into(), name: "OBS Cam".into() },
            ]
        );
    }

    #[test]
    fn only_video_devices_are_cameras() {
        assert!(Capture::camera("-f lavfi").is_err());
        assert!(Capture::camera("/etc/passwd").is_err());
    }
}
