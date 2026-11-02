//! Screen capture on Linux: a display through X11 (FFmpeg's `x11grab`), and what the computer
//! plays through PulseAudio (or PipeWire's) speaker monitor. One window alone isn't here yet.

use crate::ffsrc::{Live, LiveSound};
use crate::{AvError, Picture};

/// A display that can be captured (`id`: its place in X11's list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Display {
    pub id: u32,
    pub width: u32,
    pub height: u32,
}

/// A window that can be captured (none listed on Linux yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub id: u32,
    pub title: String,
    pub app: String,
}

/// An app with a window open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub id: String,
    pub name: String,
}

/// What to capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Display(u32),
    Window(u32),
}

/// The displays from `xrandr --listmonitors` (" 0: +*DP-1 2560/597x1440/336+0+0  DP-1"):
/// where each sits, (x, y, width, height).
pub fn parse_monitors(text: &str) -> Vec<(i32, i32, u32, u32)> {
    text.lines()
        .filter_map(|l| {
            let geo = l.split_whitespace().nth(2)?;
            let (w, rest) = geo.split_once('x')?;
            let w: u32 = w.split('/').next()?.parse().ok()?;
            let mut parts = rest.split('+');
            let h: u32 = parts.next()?.split('/').next()?.parse().ok()?;
            let x: i32 = parts.next()?.parse().ok()?;
            let y: i32 = parts.next()?.parse().ok()?;
            Some((x, y, w, h))
        })
        .collect()
}

fn monitors() -> Vec<(i32, i32, u32, u32)> {
    parse_monitors(&super::output_of("xrandr", &["--listmonitors"]))
}

/// What can be captured now: the displays (windows aren't listed on Linux yet).
pub fn shareable() -> Result<(Vec<Display>, Vec<Window>), AvError> {
    let displays =
        monitors().iter().enumerate().map(|(i, m)| Display { id: i as u32, width: m.2, height: m.3 }).collect();
    Ok((displays, Vec::new()))
}

/// The apps with a window open (none listed on Linux yet).
pub fn apps() -> Result<Vec<App>, AvError> {
    Ok(Vec::new())
}

/// A running capture; dropping it stops.
#[derive(Debug)]
pub struct ScreenCapture {
    live: Option<Live>,
    sound: Option<LiveSound>,
}

/// FFmpeg's `x11grab` input for the X display `x` (":0"), the area (x, y, w, h) or all of it.
fn x11grab(x: &str, area: Option<(i32, i32, u32, u32)>, fps: u32, cursor: bool) -> Vec<String> {
    let mut a: Vec<String> = ["-f", "x11grab", "-framerate"].map(String::from).to_vec();
    a.push(fps.to_string());
    a.extend(["-draw_mouse".into(), if cursor { "1" } else { "0" }.into()]);
    match area {
        Some((ax, ay, w, h)) => {
            a.extend(["-video_size".into(), format!("{w}x{h}"), "-i".into(), format!("{x}+{ax},{ay}")]);
        }
        None => a.extend(["-i".into(), x.to_string()]),
    }
    a
}

impl ScreenCapture {
    pub fn start(target: Target, width: u32, height: u32, fps: u32, cursor: bool) -> Result<Self, AvError> {
        Self::start_with(target, width, height, fps, cursor, false, &[])
    }

    /// Starts capturing at up to `width`×`height`, `fps` a second; with `sound`, what the
    /// computer plays too. A 2×2 (or smaller) picture is sound only (Desktop Audio). `hide`
    /// (apps left out) isn't possible here.
    pub fn start_with(
        target: Target,
        width: u32,
        height: u32,
        fps: u32,
        cursor: bool,
        sound: bool,
        hide: &[String],
    ) -> Result<Self, AvError> {
        if !hide.is_empty() {
            tracing::info!("the privacy guard leaves apps out on a Mac only: on Linux the whole screen shows");
        }
        let sound = if sound {
            let input = ["-f", "pulse", "-i", "@DEFAULT_MONITOR@"].map(String::from);
            Some(LiveSound::start(&input)?)
        } else {
            None
        };
        if width <= 2 && height <= 2 {
            return Ok(ScreenCapture { live: None, sound });
        }
        let Target::Display(i) = target else {
            return Err(AvError::Source("capturing one window works on a Mac and Windows for now".into()));
        };
        if std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland") {
            tracing::info!("on Wayland, screen capture shows only the apps that run through XWayland");
        }
        let x = std::env::var("DISPLAY").ok().filter(|d| !d.is_empty()).unwrap_or_else(|| ":0".into());
        let m = monitors();
        // The display picked; all of the screen when X11 can't say where it is.
        let area = match m.get(i as usize) {
            Some(r) => Some(*r),
            None if m.is_empty() => None,
            None => return Err(AvError::Source(format!("display {} isn't connected", i + 1))),
        };
        let live =
            Live::start(&x11grab(&x, area, fps.clamp(1, 120), cursor), (width.max(16), height.max(16)), fps, None)?;
        Ok(ScreenCapture { live: Some(live), sound })
    }

    /// The newest frame (taken).
    pub fn take(&self) -> Option<Picture> {
        self.live.as_ref()?.take().map(|(p, _)| p)
    }

    /// The computer's sound since the last call (when started with sound).
    pub fn take_sound(&self) -> Vec<(Vec<f32>, u64)> {
        self.sound.as_ref().map(LiveSound::take_sound).unwrap_or_default()
    }

    pub fn frames(&self) -> u64 {
        self.live.as_ref().map_or(0, Live::frames)
    }

    /// Why the capture ended, if it did.
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
    fn xrandr_monitors_are_read() {
        let text =
            "Monitors: 2\n 0: +*DP-1 2560/597x1440/336+0+0  DP-1\n 1: +HDMI-1 1920/527x1080/296+2560+180  HDMI-1\n";
        assert_eq!(parse_monitors(text), vec![(0, 0, 2560, 1440), (2560, 180, 1920, 1080)]);
        assert!(parse_monitors("").is_empty());
    }

    #[test]
    fn x11grab_takes_a_display_or_all() {
        let a = x11grab(":1", Some((2560, 180, 1920, 1080)), 30, false).join(" ");
        assert_eq!(a, "-f x11grab -framerate 30 -draw_mouse 0 -video_size 1920x1080 -i :1+2560,180");
        assert!(x11grab(":0", None, 60, true).join(" ").ends_with("-draw_mouse 1 -i :0"));
    }
}
