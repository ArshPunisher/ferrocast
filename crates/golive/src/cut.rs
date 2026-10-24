//! Cutting the vertical picture from the normal stream (docs/plan/LIVE-STUDIO.md A2.4): for
//! streamers without OBS's vertical canvas. The streamer marks the camera and the game on the
//! normal picture and picks a layout; ffmpeg crops, scales and stacks them into an upright
//! 1080×1920 picture, encodes it (on the graphics card when there's one) and publishes it to
//! Ferrocast's own vertical key, so everything built for the vertical canvas carries it on.
//!
//! This module is the pure part: the layout, the ffmpeg filter graph and arguments, the encoder
//! choice and the FLV bytes ffmpeg reads. The engine runs the process.

use serde::{Deserialize, Serialize};

/// A box on the normal picture, as fractions of its width and height (0 to 1), so it keeps
/// fitting when OBS's resolution changes. Advanced mode shows it in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CutBox {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl CutBox {
    /// In whole, even pixels of a `src_w`×`src_h` picture, kept inside it (encoders want even
    /// sizes).
    pub fn pixels(&self, src_w: u32, src_h: u32) -> (u32, u32, u32, u32) {
        // Never smaller than 2×2, so the bounds below always hold.
        let (src_w, src_h) = (src_w.max(2), src_h.max(2));
        let even = |v: f64| ((v.max(0.0) as u32) / 2) * 2;
        let w = even(self.w * f64::from(src_w)).clamp(2, src_w - src_w % 2);
        let h = even(self.h * f64::from(src_h)).clamp(2, src_h - src_h % 2);
        let x = even(self.x * f64::from(src_w)).min(src_w.saturating_sub(w));
        let y = even(self.y * f64::from(src_h)).min(src_h.saturating_sub(h));
        (x, y, w, h)
    }

    fn check(&self, what: &str) -> Result<(), String> {
        let ok = |v: f64| v.is_finite() && (0.0..=1.0).contains(&v);
        if !(ok(self.x) && ok(self.y) && ok(self.w) && ok(self.h)) || self.w < 0.02 || self.h < 0.02 {
            return Err(format!("{what}: the box must be inside the picture and not tiny."));
        }
        if self.x + self.w > 1.0001 || self.y + self.h > 1.0001 {
            return Err(format!("{what}: the box goes past the edge of the picture."));
        }
        Ok(())
    }
}

/// How the camera and game are stacked (board item 5A).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    /// Camera across the top, game below.
    #[default]
    CameraTop,
    /// The game fills the picture, a small camera in the top corner.
    SmallCamera,
    /// Camera and game, half each.
    HalfHalf,
    /// Only the game.
    GameOnly,
}

/// The streamer's cut: what to take from the normal picture and how to stack it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CutConfig {
    pub layout: Layout,
    /// Not used by `GameOnly`.
    pub camera: CutBox,
    pub game: CutBox,
    /// `CameraTop`: the camera's share of the height, in percent (20 to 60).
    #[serde(default = "default_share")]
    pub camera_share: u8,
    /// Output size; 1080×1920 or 720×1280.
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default = "default_fps")]
    pub fps: u32,
    /// Video bitrate in kbps (audio is copied from the normal stream).
    #[serde(default = "default_kbps")]
    pub kbps: u32,
}

fn default_share() -> u8 {
    38
}
fn default_width() -> u32 {
    1080
}
fn default_height() -> u32 {
    1920
}
fn default_fps() -> u32 {
    30
}
fn default_kbps() -> u32 {
    4500
}

impl Default for CutConfig {
    /// A 16:9 stream with the camera in the top-right corner, as most streamers have it.
    fn default() -> Self {
        Self {
            layout: Layout::CameraTop,
            camera: CutBox { x: 0.74, y: 0.04, w: 0.24, h: 0.31 },
            game: CutBox { x: 0.22, y: 0.0, w: 0.56, h: 1.0 },
            camera_share: default_share(),
            width: default_width(),
            height: default_height(),
            fps: default_fps(),
            kbps: default_kbps(),
        }
    }
}

impl CutConfig {
    /// Every field checked; messages name the field.
    pub fn validate(&self) -> Result<(), String> {
        self.game.check("game")?;
        if self.layout != Layout::GameOnly {
            self.camera.check("camera")?;
        }
        if !(20..=60).contains(&self.camera_share) {
            return Err("camera_share: 20 to 60 percent of the height.".into());
        }
        if !matches!((self.width, self.height), (1080, 1920) | (720, 1280)) {
            return Err("size: 1080×1920 or 720×1280.".into());
        }
        if !matches!(self.fps, 30 | 60) {
            return Err("fps: 30 or 60.".into());
        }
        if !(1_000..=12_000).contains(&self.kbps) {
            return Err("kbps: 1,000 to 12,000.".into());
        }
        Ok(())
    }

    /// The ffmpeg `-filter_complex` graph for a `src_w`×`src_h` picture; its output is `[v]`.
    pub fn filter_graph(&self, src_w: u32, src_h: u32) -> String {
        let (w, h, fps) = (self.width, self.height, self.fps);
        let crop = |b: &CutBox| {
            let (x, y, cw, ch) = b.pixels(src_w, src_h);
            format!("crop={cw}:{ch}:{x}:{y}")
        };
        // Fill the slot (scale up to cover it, then trim the overflow): no bars, no stretching.
        let fill = |sw: u32, sh: u32| format!("scale={sw}:{sh}:force_original_aspect_ratio=increase,crop={sw}:{sh}");
        let tail = format!("fps={fps},format=yuv420p[v]");
        match self.layout {
            Layout::GameOnly => format!("[0:v]{},{},{tail}", crop(&self.game), fill(w, h)),
            Layout::CameraTop | Layout::HalfHalf => {
                let share = if self.layout == Layout::HalfHalf { 50 } else { u32::from(self.camera_share) };
                let top = (h * share / 100) / 2 * 2;
                let bottom = h - top;
                format!(
                    "[0:v]split=2[c][g];[c]{},{}[top];[g]{},{}[bottom];[top][bottom]vstack=inputs=2,{tail}",
                    crop(&self.camera),
                    fill(w, top),
                    crop(&self.game),
                    fill(w, bottom)
                )
            }
            Layout::SmallCamera => {
                // The camera keeps its shape at 42% of the width, in the top-right corner.
                let (_, _, cw, ch) = self.camera.pixels(src_w, src_h);
                let pw = (w * 42 / 100) / 2 * 2;
                let ph = ((u64::from(pw) * u64::from(ch) / u64::from(cw.max(1))) as u32 / 2 * 2).max(2);
                let (ox, oy) = (w - pw - w * 4 / 100, h * 3 / 100);
                format!(
                    "[0:v]split=2[c][g];[g]{},{}[bg];[c]{},scale={pw}:{ph}[cam];[bg][cam]overlay={ox}:{oy},{tail}",
                    crop(&self.game),
                    fill(w, h),
                    crop(&self.camera)
                )
            }
        }
    }
}

/// A video encoder ffmpeg may have, best first. Hardware ones leave the CPU to the game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Encoder {
    pub name: &'static str,
    /// Shown to the streamer ("your graphics card", "the processor").
    pub on: &'static str,
}

/// The encoders to try on this OS, best first; the software one always works.
pub fn encoder_candidates() -> &'static [Encoder] {
    const GPU: &str = "your graphics card";
    if cfg!(target_os = "macos") {
        &[Encoder { name: "h264_videotoolbox", on: GPU }, Encoder { name: "libx264", on: "the processor" }]
    } else if cfg!(target_os = "windows") {
        &[
            Encoder { name: "h264_nvenc", on: GPU },
            Encoder { name: "h264_qsv", on: GPU },
            Encoder { name: "h264_amf", on: GPU },
            Encoder { name: "libx264", on: "the processor" },
        ]
    } else {
        &[Encoder { name: "h264_nvenc", on: GPU }, Encoder { name: "libx264", on: "the processor" }]
    }
}

/// The ffmpeg arguments: the normal stream as FLV on stdin, the vertical picture published to
/// `publish` (Ferrocast's own vertical key), audio copied.
pub fn ffmpeg_args(cfg: &CutConfig, src_w: u32, src_h: u32, encoder: &Encoder, publish: &str) -> Vec<String> {
    let k = cfg.kbps;
    let mut a: Vec<String> = [
        "-hide_banner",
        "-loglevel",
        "warning",
        "-nostdin",
        "-fflags",
        "+genpts",
        // The headers come first, so a second of looking is plenty (ffmpeg's default is 5 s).
        "-analyzeduration",
        "1000000",
        "-probesize",
        "1000000",
        "-f",
        "flv",
        "-i",
        "pipe:0",
        "-filter_complex",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    a.push(cfg.filter_graph(src_w, src_h));
    a.extend(["-map", "[v]", "-map", "0:a?", "-c:v", encoder.name].iter().map(|s| (*s).to_string()));
    a.extend([
        "-b:v".to_string(),
        format!("{k}k"),
        "-maxrate".to_string(),
        format!("{k}k"),
        "-bufsize".to_string(),
        format!("{}k", k * 2),
        // A keyframe every 2 s, as YouTube and the others want.
        "-g".to_string(),
        (cfg.fps * 2).to_string(),
    ]);
    if encoder.name == "libx264" {
        a.extend(["-preset", "veryfast", "-tune", "zerolatency"].iter().map(|s| (*s).to_string()));
    }
    if encoder.name == "h264_videotoolbox" {
        a.extend(["-realtime", "1"].iter().map(|s| (*s).to_string()));
    }
    a.extend(["-c:a", "copy", "-f", "flv", publish].iter().map(|s| (*s).to_string()));
    a
}

/// FLV's file header: version 1, audio and video, followed by the first "previous tag size".
pub fn flv_header() -> [u8; 13] {
    [b'F', b'L', b'V', 1, 0x05, 0, 0, 0, 9, 0, 0, 0, 0]
}

/// One FLV tag (8 audio, 9 video, 18 script data) and the "previous tag size" after it.
pub fn flv_tag(kind: u8, ts: u32, data: &[u8]) -> Vec<u8> {
    let len = data.len() as u32;
    let mut t = Vec::with_capacity(data.len() + 15);
    t.push(kind);
    t.extend_from_slice(&len.to_be_bytes()[1..]);
    t.extend_from_slice(&ts.to_be_bytes()[1..]);
    t.push((ts >> 24) as u8);
    t.extend_from_slice(&[0, 0, 0]);
    t.extend_from_slice(data);
    t.extend_from_slice(&(len + 11).to_be_bytes());
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boxes_become_even_pixels_inside_the_picture() {
        let b = CutBox { x: 0.74, y: 0.04, w: 0.24, h: 0.31 };
        assert_eq!(b.pixels(2560, 1440), (1894, 56, 614, 446));
        assert_eq!(b.pixels(0, 1), (0, 0, 2, 2), "a nonsense size doesn't panic");
        // Past the edge: pulled back in.
        let edge = CutBox { x: 0.9, y: 0.9, w: 0.2, h: 0.2 };
        let (x, y, w, h) = edge.pixels(1920, 1080);
        assert!(x + w <= 1920 && y + h <= 1080 && w % 2 == 0 && h % 2 == 0);
    }

    #[test]
    fn graphs_for_each_layout() {
        let c = CutConfig::default();
        assert_eq!(
            c.filter_graph(1920, 1080),
            "[0:v]split=2[c][g];[c]crop=460:334:1420:42,scale=1080:728:force_original_aspect_ratio=increase,crop=1080:728[top];\
             [g]crop=1074:1080:422:0,scale=1080:1192:force_original_aspect_ratio=increase,crop=1080:1192[bottom];\
             [top][bottom]vstack=inputs=2,fps=30,format=yuv420p[v]"
        );
        let game = CutConfig { layout: Layout::GameOnly, ..c.clone() };
        assert!(game.filter_graph(1920, 1080).starts_with("[0:v]crop=1074:1080:422:0,scale=1080:1920:"));
        let half = CutConfig { layout: Layout::HalfHalf, ..c.clone() };
        assert!(half.filter_graph(1920, 1080).contains("scale=1080:960:"));
        let small = CutConfig { layout: Layout::SmallCamera, ..c.clone() };
        let g = small.filter_graph(1920, 1080);
        assert!(g.contains("scale=452:328[cam]") && g.contains("overlay=585:57"), "{g}");
        assert!(c.validate().is_ok());
    }

    #[test]
    fn bad_cuts_say_which_field() {
        let c = CutConfig::default();
        let tiny = CutConfig { camera: CutBox { x: 0.1, y: 0.1, w: 0.001, h: 0.2 }, ..c.clone() };
        assert!(tiny.validate().unwrap_err().starts_with("camera:"));
        let past = CutConfig { game: CutBox { x: 0.5, y: 0.0, w: 0.7, h: 1.0 }, ..c.clone() };
        assert!(past.validate().unwrap_err().starts_with("game:"));
        // GameOnly ignores the camera box.
        let game_only =
            CutConfig { layout: Layout::GameOnly, camera: CutBox { x: 2.0, y: 0.0, w: 0.0, h: 0.0 }, ..c.clone() };
        assert!(game_only.validate().is_ok());
        assert!(CutConfig { fps: 50, ..c.clone() }.validate().unwrap_err().starts_with("fps:"));
        assert!(CutConfig { width: 1000, ..c.clone() }.validate().unwrap_err().starts_with("size:"));
        assert!(CutConfig { kbps: 50_000, ..c.clone() }.validate().unwrap_err().starts_with("kbps:"));
        assert!(CutConfig { camera_share: 80, ..c }.validate().unwrap_err().starts_with("camera_share:"));
    }

    #[test]
    fn ffmpeg_gets_stdin_and_publishes_to_the_vertical_key() {
        let enc = Encoder { name: "libx264", on: "the processor" };
        let a = ffmpeg_args(&CutConfig::default(), 1920, 1080, &enc, "rtmp://127.0.0.1:1935/live/fc_kv");
        let s = a.join(" ");
        assert!(s.contains("-f flv -i pipe:0 -filter_complex [0:v]split=2"));
        assert!(s.contains("-c:v libx264 -b:v 4500k -maxrate 4500k -bufsize 9000k -g 60 -preset veryfast"));
        assert!(s.ends_with("-c:a copy -f flv rtmp://127.0.0.1:1935/live/fc_kv"));
        assert_eq!(encoder_candidates().last().unwrap().name, "libx264", "software always last");
    }

    #[test]
    fn flv_bytes() {
        assert_eq!(&flv_header()[..3], b"FLV");
        let t = flv_tag(9, 0x0102_0304, &[0xAA, 0xBB]);
        assert_eq!(t, vec![9, 0, 0, 2, 0x02, 0x03, 0x04, 0x01, 0, 0, 0, 0xAA, 0xBB, 0, 0, 0, 13]);
    }
}
