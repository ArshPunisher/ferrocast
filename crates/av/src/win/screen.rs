//! Screen and window capture on Windows: a display through Desktop Duplication (FFmpeg's
//! `ddagrab`; GDI's `gdigrab` where that isn't there), one window through GDI, and what the
//! computer plays through WASAPI's loopback.

use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible};
use windows::core::BOOL;

use crate::ffsrc::Live;
use crate::sound_in::{SoundIn, Which};
use crate::{AvError, Picture};

/// A display that can be captured (`id`: its place in Windows' list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Display {
    pub id: u32,
    pub width: u32,
    pub height: u32,
}

/// A window that can be captured (`id`: its handle).
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

/// The displays with where each sits on the desktop: (x, y, width, height).
fn monitors() -> Vec<(i32, i32, u32, u32)> {
    unsafe extern "system" fn each(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the Vec this call was given below, alive for EnumDisplayMonitors.
        let list = unsafe { &mut *(data.0 as *mut Vec<(i32, i32, u32, u32)>) };
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        // SAFETY: a monitor handle Windows just gave, and an info struct with its size set.
        if unsafe { GetMonitorInfoW(m, &mut info) }.as_bool() {
            let r = info.rcMonitor;
            list.push((r.left, r.top, (r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32));
        }
        true.into()
    }
    let mut list: Vec<(i32, i32, u32, u32)> = Vec::new();
    // SAFETY: the callback reads `list` only during this call.
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(each), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

/// The exe name of the process that owns `hwnd` ("chrome").
fn app_of(hwnd: HWND) -> String {
    let mut pid = 0u32;
    // SAFETY: a window handle from EnumWindows; `pid` is ours.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    // SAFETY: opening for a query only; the handle is closed by its own drop.
    let Ok(h) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else { return String::new() };
    let mut buf = [0u16; 520];
    let mut len = buf.len() as u32;
    // SAFETY: the buffer and its length are ours.
    let ok =
        unsafe { QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut len) };
    // SAFETY: closing the handle opened above.
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(h);
    }
    if ok.is_err() {
        return String::new();
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    path.rsplit(['\\', '/']).next().unwrap_or("").trim_end_matches(".exe").to_string()
}

fn windows() -> Vec<Window> {
    unsafe extern "system" fn each(hwnd: HWND, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the Vec given below, alive for EnumWindows.
        let list = unsafe { &mut *(data.0 as *mut Vec<Window>) };
        // SAFETY: a window handle EnumWindows gave.
        if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
            return true.into();
        }
        let mut buf = [0u16; 512];
        // SAFETY: the buffer is ours.
        let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
        if n > 0 {
            let title = String::from_utf16_lossy(&buf[..n as usize]);
            list.push(Window { id: hwnd.0 as usize as u32, title, app: app_of(hwnd) });
        }
        true.into()
    }
    let mut list: Vec<Window> = Vec::new();
    // SAFETY: the callback reads `list` only during this call.
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

/// What can be captured now: displays, and visible windows with a title.
pub fn shareable() -> Result<(Vec<Display>, Vec<Window>), AvError> {
    let displays =
        monitors().iter().enumerate().map(|(i, m)| Display { id: i as u32, width: m.2, height: m.3 }).collect();
    Ok((displays, windows()))
}

/// The apps with a window open, by name.
pub fn apps() -> Result<Vec<App>, AvError> {
    let mut names: Vec<String> = windows().into_iter().map(|w| w.app).filter(|a| !a.is_empty()).collect();
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    Ok(names.into_iter().map(|n| App { id: n.clone(), name: n }).collect())
}

/// A running capture; dropping it stops.
#[derive(Debug)]
pub struct ScreenCapture {
    live: Option<Live>,
    sound: Option<SoundIn>,
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

impl ScreenCapture {
    pub fn start(target: Target, width: u32, height: u32, fps: u32, cursor: bool) -> Result<Self, AvError> {
        Self::start_with(target, width, height, fps, cursor, false, &[])
    }

    /// Starts capturing at up to `width`×`height`, `fps` a second; with `sound`, what the
    /// computer plays too. A 2×2 (or smaller) picture is sound only (Desktop Audio).
    /// `hide` (apps left out) isn't possible here: Windows can't leave other apps out.
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
            tracing::info!("the privacy guard leaves apps out on a Mac only: on Windows the whole screen shows");
        }
        let sound = if sound { Some(SoundIn::start(Which::Loopback)?) } else { None };
        if width <= 2 && height <= 2 {
            return Ok(ScreenCapture { live: None, sound });
        }
        let (fps, mouse) = (fps.clamp(1, 120), if cursor { "1" } else { "0" });
        let max = (width.max(16), height.max(16));
        let live = match target {
            Target::Display(i) => {
                // Desktop Duplication first (the GPU's copy, fast), else GDI.
                let dda = args(&["-f", "lavfi", "-i"])
                    .into_iter()
                    .chain([format!(
                        "ddagrab=output_idx={i}:framerate={fps}:draw_mouse={mouse},hwdownload,format=bgra"
                    )])
                    .collect::<Vec<_>>();
                match Live::start(&dda, max, fps, None) {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::info!(error = %e, "Desktop Duplication isn't there: GDI reads the display");
                        let m = monitors();
                        let (x, y, w, h) = *m
                            .get(i as usize)
                            .ok_or_else(|| AvError::Source(format!("display {} isn't connected", i + 1)))?;
                        let gdi = vec![
                            "-f".into(),
                            "gdigrab".into(),
                            "-framerate".into(),
                            fps.to_string(),
                            "-draw_mouse".into(),
                            mouse.into(),
                            "-offset_x".into(),
                            x.to_string(),
                            "-offset_y".into(),
                            y.to_string(),
                            "-video_size".into(),
                            format!("{w}x{h}"),
                            "-i".into(),
                            "desktop".into(),
                        ];
                        Live::start(&gdi, max, fps, None)?
                    }
                }
            }
            Target::Window(id) => {
                let w = windows()
                    .into_iter()
                    .find(|w| w.id == id)
                    .ok_or_else(|| AvError::Source("that window is closed".into()))?;
                let gdi = vec![
                    "-f".into(),
                    "gdigrab".into(),
                    "-framerate".into(),
                    fps.to_string(),
                    "-draw_mouse".into(),
                    mouse.into(),
                    "-i".into(),
                    format!("title={}", w.title),
                ];
                Live::start(&gdi, max, fps, None)?
            }
        };
        Ok(ScreenCapture { live: Some(live), sound })
    }

    /// The newest frame (taken).
    pub fn take(&self) -> Option<Picture> {
        self.live.as_ref()?.take().map(|(p, _)| p)
    }

    /// The computer's sound since the last call (when started with sound).
    pub fn take_sound(&self) -> Vec<(Vec<f32>, u64)> {
        self.sound.as_ref().map(SoundIn::take_sound).unwrap_or_default()
    }

    pub fn frames(&self) -> u64 {
        self.live.as_ref().map_or(0, Live::frames)
    }

    /// Why FFmpeg's capture ended, if it did.
    pub fn ended(&self) -> Option<String> {
        self.live.as_ref()?.ended()
    }
}
