//! The virtual camera on Linux: what's on air (or one source) handed to a v4l2loopback camera
//! through FFmpeg, so video calls and browsers can pick it as OBS's does. The camera itself is
//! the v4l2loopback module (`sudo modprobe v4l2loopback exclusive_caps=1 card_label="Ferrocast
//! Camera"`); a Mac and Windows need a signed camera of their own, not here yet.

use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::own::Shared;

/// The first v4l2loopback camera under `sys` (`/sys/class/video4linux`): its `/dev/videoN`.
pub(crate) fn loopback_in(sys: &Path) -> Option<String> {
    let mut found: Vec<String> = std::fs::read_dir(sys)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            // v4l2loopback's devices name their driver in their sysfs folder, or carry its
            // default name or the label they were given (OBS's, ours).
            let name = std::fs::read_to_string(e.path().join("name")).unwrap_or_default().to_lowercase();
            let driver = std::fs::read_link(e.path().join("device").join("driver"))
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_default();
            driver == "v4l2loopback"
                || ["dummy video device", "v4l2loopback", "virtual camera", "ferrocast camera"]
                    .iter()
                    .any(|n| name.contains(n))
        })
        .map(|e| format!("/dev/{}", e.file_name().to_string_lossy()))
        .collect();
    found.sort();
    found.into_iter().next()
}

/// A running virtual camera; dropping it stops.
#[derive(Debug)]
pub(crate) struct Loopback {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Loopback {
    pub(crate) fn active(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Starts sending what's on air (or `source` alone) at `size`, `fps` a second.
    pub(crate) fn start(
        engine: Weak<Shared>,
        source: Option<String>,
        size: (u32, u32),
        fps: u32,
    ) -> Result<Loopback, String> {
        let dev = loopback_in(Path::new("/sys/class/video4linux")).ok_or(
            "There's no virtual camera device: install v4l2loopback and run \
             `sudo modprobe v4l2loopback exclusive_caps=1 card_label=\"Ferrocast Camera\"`.",
        )?;
        let ffmpeg = ferrocore::ffenc::path()
            .ok_or("The virtual camera needs FFmpeg: pick x264 once in Settings and Ferrocast fetches it.")?;
        let (w, h) = (size.0.max(2) & !1, size.1.max(2) & !1);
        let fps = fps.clamp(1, 60);
        let mut child: Child = Command::new(ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "rgba", "-s"])
            .arg(format!("{w}x{h}"))
            .args(["-r", &fps.to_string(), "-i", "-", "-f", "v4l2", "-pix_fmt", "yuv420p", &dev])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("The virtual camera couldn't start FFmpeg ({e})."))?;
        let mut pipe = child.stdin.take().ok_or("The virtual camera couldn't start FFmpeg.")?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("ferrocast-virtualcam".into())
                .spawn(move || {
                    let every = Duration::from_secs(1) / fps;
                    let mut next = Instant::now();
                    while !stop.load(Ordering::Acquire) {
                        let Some(shared) = engine.upgrade() else { break };
                        let e = &shared.engine;
                        let frame = match &source {
                            Some(name) => e.source_named(name).and_then(|(id, _)| e.snapshot_source_at(id, (w, h))),
                            None => e.snapshot().ok(),
                        };
                        drop(shared);
                        // A frame of the right size, else black (never a torn one).
                        let frame = frame
                            .filter(|f| f.len() == (w * h * 4) as usize)
                            .unwrap_or_else(|| [0u8, 0, 0, 255].repeat((w * h) as usize));
                        if pipe.write_all(&frame).is_err() {
                            tracing::warn!("the virtual camera's FFmpeg stopped");
                            break;
                        }
                        next += every;
                        if let Some(rest) = next.checked_duration_since(Instant::now()) {
                            std::thread::sleep(rest);
                        } else {
                            next = Instant::now();
                        }
                    }
                    drop(pipe);
                    let _ = child.kill();
                    let _ = child.wait();
                })
                .map_err(|e| format!("The virtual camera couldn't start ({e})."))?
        };
        Ok(Loopback { stop, thread: Some(thread) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_loopback_camera() {
        let dir = std::env::temp_dir().join(format!("ferrocast-v4l-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (node, name) in
            [("video0", "Integrated Camera"), ("video4", "Ferrocast Camera"), ("video9", "Dummy video device (0x0000)")]
        {
            let d = dir.join(node);
            assert!(std::fs::create_dir_all(&d).and_then(|()| std::fs::write(d.join("name"), name)).is_ok());
        }
        let got = loopback_in(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got.as_deref(), Some("/dev/video4"));
        assert_eq!(loopback_in(Path::new("/nowhere")), None);
    }
}
