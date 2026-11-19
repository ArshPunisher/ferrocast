//! Linux: screens (X11, FFmpeg's `x11grab`), cameras (Video4Linux), microphones and what the
//! computer plays (PulseAudio, or PipeWire's PulseAudio service) and video files, all read by
//! FFmpeg. On Wayland, `x11grab` sees only the apps that run through XWayland.

pub mod camera;
pub mod screen;

/// Video and sound files.
#[cfg(target_os = "linux")]
pub mod media {
    pub use crate::ffsrc::{Control, Media, MediaState};
}

/// What a command printed, or nothing when it isn't there.
fn output_of(program: &str, args: &[&str]) -> String {
    std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}
