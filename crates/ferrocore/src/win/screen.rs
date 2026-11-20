//! Screen and window capture on Windows: Windows' own capture first (games in full screen too),
//! else a display through Desktop Duplication (FFmpeg's `ddagrab`; GDI's `gdigrab` where that
//! isn't there) and a window through GDI; and what the computer plays through WASAPI's loopback.

use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    DISPLAY_DEVICEW, EnumDisplayDevicesW, EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
    MONITORINFOEXW,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EDD_GET_DEVICE_INTERFACE_NAME, EnumWindows, GWL_EXSTYLE, GWL_STYLE, GetWindowLongW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindow, IsWindowVisible, WS_CHILD, WS_EX_TOOLWINDOW,
};
use windows::core::BOOL;

use crate::ffsrc::Live;
use crate::sound_in::{Heal, SoundIn, Which};
use crate::win_logic::{WinMonitor, pick_win_monitor, strip_exe_suffix};
use crate::{AvError, Picture};

/// A frame of the screen as BGRA, sent to the card without a colour swap.
pub use super::wgc::Surface;

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

struct MonitorEntry {
    handle: HMONITOR,
    rect: (i32, i32, u32, u32),
    id: WinMonitor,
}

/// All connected displays with their handle, desktop rect, and stable `monitor_id`.
fn monitor_entries() -> Vec<MonitorEntry> {
    unsafe extern "system" fn each(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the Vec this call was given below, alive for EnumDisplayMonitors.
        let list = unsafe { &mut *(data.0 as *mut Vec<MonitorEntry>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        // SAFETY: `MONITORINFOEXW` starts with `MONITORINFO` and its `cbSize` is set to the extended struct.
        if unsafe { GetMonitorInfoW(m, (&raw mut info).cast::<MONITORINFO>()) }.as_bool() {
            let r = info.monitorInfo.rcMonitor;
            let sz_len = info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len());
            let device_name = String::from_utf16_lossy(&info.szDevice[..sz_len]);
            let mut dev = DISPLAY_DEVICEW { cb: size_of::<DISPLAY_DEVICEW>() as u32, ..Default::default() };
            // SAFETY: `info.szDevice` is null-terminated and `dev.cb` is set.
            let got = unsafe {
                EnumDisplayDevicesW(
                    windows::core::PCWSTR(info.szDevice.as_ptr()),
                    0,
                    &raw mut dev,
                    EDD_GET_DEVICE_INTERFACE_NAME,
                )
            }
            .as_bool();
            let monitor_id = if got {
                let id_len = dev.DeviceID.iter().position(|&c| c == 0).unwrap_or(dev.DeviceID.len());
                String::from_utf16_lossy(&dev.DeviceID[..id_len])
            } else {
                device_name.clone()
            };
            list.push(MonitorEntry {
                handle: m,
                rect: (r.left, r.top, (r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32),
                id: WinMonitor { monitor_id, device_name },
            });
        }
        true.into()
    }
    let mut list: Vec<MonitorEntry> = Vec::new();
    // SAFETY: the callback reads `list` only during this call.
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(each), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

/// Stable device path (`monitor_id`) of display index `id`, if connected.
pub fn display_uuid(id: u32) -> Option<String> {
    let entries = monitor_entries();
    let entry = entries.get(id as usize)?;
    let out = if entry.id.monitor_id.is_empty() { &entry.id.device_name } else { &entry.id.monitor_id };
    (!out.is_empty()).then(|| out.clone())
}

/// Current index of display `saved`, preferring its stable `monitor_id` (`uuid`).
pub fn display_now(saved: u32, uuid: Option<&str>) -> Option<u32> {
    let entries = monitor_entries();
    let ids: Vec<WinMonitor> = entries.iter().map(|e| e.id.clone()).collect();
    pick_win_monitor(saved, uuid, &ids)
}

fn dxgi_output_index(monitor: HMONITOR) -> Option<u32> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    // SAFETY: creates a DXGI factory to locate which output on the primary adapter owns `monitor`.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1().ok()? };
    // SAFETY: adapter 0 enumeration.
    let adapter = unsafe { factory.EnumAdapters1(0).ok()? };
    let mut idx = 0u32;
    // SAFETY: output enumeration and descriptor read on adapter 0.
    while let Ok(output) = unsafe { adapter.EnumOutputs(idx) } {
        // SAFETY: output is valid; GetDesc reads into a stack struct.
        if let Ok(desc) = unsafe { output.GetDesc() }
            && desc.Monitor == monitor
        {
            return Some(idx);
        }
        idx += 1;
    }
    None
}

/// The exe name of the process that owns `hwnd` ("chrome").
/// Who owns window `id` now (its program's name): `Some(None)` once it's closed. A window on
/// another virtual desktop, or minimized, still counts (the window list leaves those out). A
/// handle can come back for another program's window, so its program says it's the same one.
pub fn window_owner(id: u32) -> Option<Option<String>> {
    let hwnd = HWND(id as usize as *mut _);
    // SAFETY: IsWindow only looks the handle up.
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return Some(None);
    }
    Some(Some(app_of(hwnd)))
}

fn app_of(hwnd: HWND) -> String {
    let mut pid = 0u32;
    // SAFETY: a window handle from EnumWindows; `pid` is ours.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    // SAFETY: opening for a query only; the handle is closed unconditionally below.
    let Ok(h) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else { return String::new() };
    let mut buf = vec![0u16; 520];
    let mut path_len = None;
    loop {
        let mut len = buf.len() as u32;
        // SAFETY: `buf` is uniquely owned and `len` matches its capacity.
        match unsafe {
            QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut len)
        } {
            Ok(()) => {
                path_len = Some(len as usize);
                break;
            }
            Err(e) if e.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult() && buf.len() < 32_768 => {
                buf.resize((buf.len() * 2).min(32_768), 0);
            }
            Err(_) => break,
        }
    }
    // SAFETY: closing the handle opened above.
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(h);
    }
    let Some(len) = path_len else {
        return String::new();
    };
    let path = String::from_utf16_lossy(&buf[..len]);
    strip_exe_suffix(path.rsplit(['\\', '/']).next().unwrap_or("")).to_string()
}

/// A window someone would pick, by OBS's rules: not a tool window (tray helpers, overlays like
/// NVIDIA's, the desktop's "Program Manager", our own single-instance window), not a child,
/// and not cloaked (a Store app's frame while it's suspended, Windows' input panel, a window
/// on another desktop). On 10 Nov the list showed all of those.
fn pickable(hwnd: HWND) -> bool {
    // SAFETY: a window handle EnumWindows gave; reading its styles changes nothing.
    let (style, ex) = unsafe { (GetWindowLongW(hwnd, GWL_STYLE) as u32, GetWindowLongW(hwnd, GWL_EXSTYLE) as u32) };
    if ex & WS_EX_TOOLWINDOW.0 != 0 || style & WS_CHILD.0 != 0 {
        return false;
    }
    let mut cloaked = 0u32;
    // SAFETY: a u32 for DWMWA_CLOAKED to fill, with its size given.
    let got = unsafe {
        DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&mut cloaked as *mut u32).cast(), size_of::<u32>() as u32)
    };
    got.is_err() || cloaked == 0
}

/// The visible windows with a title; `pick`: only those someone would pick (`pickable`).
fn windows_of(pick: bool) -> Vec<Window> {
    struct Found {
        pick: bool,
        list: Vec<Window>,
    }
    unsafe extern "system" fn each(hwnd: HWND, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the `Found` given below, alive for EnumWindows.
        let found = unsafe { &mut *(data.0 as *mut Found) };
        // SAFETY: a window handle EnumWindows gave.
        if !unsafe { IsWindowVisible(hwnd) }.as_bool() || (found.pick && !pickable(hwnd)) {
            return true.into();
        }
        let mut buf = [0u16; 512];
        // SAFETY: the buffer is ours.
        let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
        if n > 0 {
            let title = String::from_utf16_lossy(&buf[..n as usize]);
            found.list.push(Window { id: hwnd.0 as usize as u32, title, app: app_of(hwnd) });
        }
        true.into()
    }
    let mut found = Found { pick, list: Vec::new() };
    // SAFETY: the callback reads `found` only during this call.
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut found as *mut _ as isize));
    }
    found.list
}

/// Every visible window with a title (to find one that was picked, whatever it is now).
fn windows() -> Vec<Window> {
    windows_of(false)
}

/// What can be captured now: displays, and the windows someone would pick.
pub fn shareable() -> Result<(Vec<Display>, Vec<Window>), AvError> {
    let displays = monitor_entries()
        .iter()
        .enumerate()
        .map(|(i, m)| Display { id: i as u32, width: m.rect.2, height: m.rect.3 })
        .collect();
    Ok((displays, windows_of(true)))
}

/// The apps with a window someone would pick, by name.
pub fn apps() -> Result<Vec<App>, AvError> {
    let mut names: Vec<String> = windows_of(true).into_iter().map(|w| w.app).filter(|a| !a.is_empty()).collect();
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    Ok(names.into_iter().map(|n| App { id: n.clone(), name: n }).collect())
}

/// Where the pictures come from.
#[derive(Debug)]
enum Pictures {
    Native(super::wgc::Wgc),
    Ffmpeg(Live),
}

/// A running capture; dropping it stops.
#[derive(Debug)]
pub struct ScreenCapture {
    live: Option<Pictures>,
    /// With a picture, its sound starts itself again when it stops (the picture goes on); sound
    /// alone says it stopped and the watchdog starts it again.
    sound: Option<Heal>,
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

fn missing_display_error(i: u32) -> AvError {
    if i == u32::MAX {
        AvError::Source("that display isn't connected (plug it in, or pick the display again)".into())
    } else {
        AvError::Source(format!("display {} isn't connected (plug it in, or pick the display again)", i + 1))
    }
}

fn start_gdi_display(entry: &MonitorEntry, max: (u32, u32), fps: u32, mouse: &str) -> Result<Live, AvError> {
    let (x, y, w, h) = entry.rect;
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
    Live::start(&gdi, max, fps, None)
}

impl ScreenCapture {
    pub fn start(target: Target, width: u32, height: u32, fps: u32, cursor: bool) -> Result<Self, AvError> {
        Self::start_with(target, width, height, fps, cursor, false, &[])
    }

    /// Starts capturing display `(saved, uuid)` using a single `monitor_entries()` snapshot so
    /// display resolution and handle lookup have no TOCTOU window across hotplug events.
    pub fn start_display(
        saved: u32,
        uuid: Option<&str>,
        size: (u32, u32),
        fps: u32,
        cursor: bool,
        sound: bool,
        hide: &[String],
    ) -> Result<Self, AvError> {
        let entries = monitor_entries();
        let ids: Vec<WinMonitor> = entries.iter().map(|e| e.id.clone()).collect();
        let idx = pick_win_monitor(saved, uuid, &ids);
        let entry = idx
            .and_then(|i| entries.into_iter().nth(i as usize))
            .ok_or_else(|| missing_display_error(if uuid.is_some() { u32::MAX } else { saved }))?;
        Self::start_with_target(Target::Display(idx.unwrap_or(saved)), Some(entry), size, fps, cursor, sound, hide)
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
        let display_entry = match target {
            Target::Display(i) => {
                Some(monitor_entries().into_iter().nth(i as usize).ok_or_else(|| missing_display_error(i))?)
            }
            Target::Window(_) => None,
        };
        Self::start_with_target(target, display_entry, (width, height), fps, cursor, sound, hide)
    }

    fn start_with_target(
        target: Target,
        display_entry: Option<MonitorEntry>,
        (width, height): (u32, u32),
        fps: u32,
        cursor: bool,
        sound: bool,
        hide: &[String],
    ) -> Result<Self, AvError> {
        // The engine's "its window is closed" (window 0): nothing opened for it, its sound
        // either (every watchdog try opened and dropped an audio client, projects-60).
        if let Target::Window(0) = target {
            return Err(AvError::Source("its window is closed (open it, or pick the window again)".into()));
        }
        if !hide.is_empty() {
            tracing::info!("the privacy guard leaves apps out on a Mac only: on Windows the whole screen shows");
        }
        // A window's sound is its app's own (OBS's "Capture Audio"); a display's is everything.
        // An app's sound that can't be had (older Windows) leaves the picture without it.
        let sound = match (sound, target) {
            (false, _) => None,
            (true, Target::Window(id)) => match SoundIn::start(Which::Process(super::app_sound::process_of(id))) {
                Ok(s) => Some((s, Which::Process(super::app_sound::process_of(id)))),
                Err(e) => {
                    tracing::warn!(error = %e, "the window's sound can't be heard");
                    None
                }
            },
            (true, Target::Display(_)) => Some((SoundIn::start(Which::Loopback)?, Which::Loopback)),
        };
        let picture = width > 2 || height > 2;
        let sound = sound.map(|(s, which)| Heal::new(s, which, picture));
        if width <= 2 && height <= 2 {
            return Ok(ScreenCapture { live: None, sound });
        }
        let (fps, mouse) = (fps.clamp(1, 120), if cursor { "1" } else { "0" });
        let max = (width.max(16), height.max(16));
        // Windows' own capture first; FFmpeg's where it can't.
        let native = match (target, &display_entry) {
            (Target::Display(_), Some(entry)) => super::wgc::Wgc::display(entry.handle, max, fps, cursor),
            (Target::Display(i), None) => Err(missing_display_error(i)),
            (Target::Window(id), _) => super::wgc::Wgc::window(HWND(id as usize as *mut _), max, fps, cursor),
        };
        match native {
            Ok(w) => return Ok(ScreenCapture { live: Some(Pictures::Native(w)), sound }),
            Err(e) => tracing::info!(error = %e, "Windows' own capture can't: FFmpeg captures instead"),
        }
        let live = match (target, display_entry) {
            (Target::Display(_), Some(entry)) => match dxgi_output_index(entry.handle) {
                Some(dda_idx) => {
                    // Desktop Duplication first (the GPU's copy, fast), else GDI.
                    let dda = args(&["-f", "lavfi", "-i"])
                        .into_iter()
                        .chain([format!(
                            "ddagrab=output_idx={dda_idx}:framerate={fps}:draw_mouse={mouse},hwdownload,format=bgra"
                        )])
                        .collect::<Vec<_>>();
                    match Live::start(&dda, max, fps, None) {
                        Ok(l) => l,
                        Err(e) => {
                            tracing::info!(error = %e, "Desktop Duplication isn't there: GDI reads the display");
                            start_gdi_display(&entry, max, fps, mouse)?
                        }
                    }
                }
                None => {
                    // Never guess output_idx when a monitor sits on a secondary GPU adapter.
                    tracing::info!("display isn't on the primary DXGI adapter: GDI reads the display");
                    start_gdi_display(&entry, max, fps, mouse)?
                }
            },
            (Target::Display(i), None) => return Err(missing_display_error(i)),
            (Target::Window(id), _) => {
                let w = windows()
                    .into_iter()
                    .find(|w| w.id == id)
                    .ok_or_else(|| AvError::Source("that window is closed".into()))?;
                // FFmpeg's gdigrab finds a window by its title alone (the first with it, of any
                // app): with two of that title (two "New Tab" browsers, an app and its updater)
                // the other one could go on air (projects-60). Refused then.
                // FindWindow ignores case: "Report" and "report" are the same title to it.
                let low = w.title.to_lowercase();
                let same = windows_of(false).into_iter().filter(|o| o.title.to_lowercase() == low).count();
                if same > 1 {
                    return Err(AvError::Source(format!(
                        "{same} windows are called \"{}\": this capture can't tell them apart (rename or close the other)",
                        w.title
                    )));
                }
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
        Ok(ScreenCapture { live: Some(Pictures::Ffmpeg(live)), sound })
    }

    /// What the computer plays (Desktop Audio) directly via WASAPI loopback, without touching
    /// display or window enumeration so headless and monitor-off Windows PCs still work.
    pub fn start_desktop_sound() -> Result<Self, AvError> {
        let sound = SoundIn::start(Which::Loopback)?;
        Ok(ScreenCapture { live: None, sound: Some(Heal::new(sound, Which::Loopback, false)) })
    }

    /// One app's sound only (OBS's Application Audio Capture), by its program's name ("chrome"):
    /// the first open window's process of that program, with its child processes.
    pub fn start_app_sound(app: &str) -> Result<Self, AvError> {
        let w = windows()
            .into_iter()
            .find(|w| w.app.eq_ignore_ascii_case(app))
            .ok_or_else(|| AvError::Source(format!("{app} has no window open")))?;
        let which = Which::Process(super::app_sound::process_of(w.id));
        let sound = SoundIn::start(which.clone())?;
        Ok(ScreenCapture { live: None, sound: Some(Heal::new(sound, which, false)) })
    }

    /// The newest frame (taken).
    pub fn take(&self) -> Option<Picture> {
        match self.live.as_ref()? {
            Pictures::Native(w) => w.take(),
            Pictures::Ffmpeg(l) => l.take().map(|(p, _)| p),
        }
    }

    /// The newest frame of Windows' own capture as BGRA (taken); None from FFmpeg's (`take`).
    pub fn take_surface(&self) -> Option<Surface> {
        match self.live.as_ref()? {
            Pictures::Native(w) => w.take_surface(),
            Pictures::Ffmpeg(_) => None,
        }
    }

    /// The computer's sound since the last call (when started with sound).
    pub fn take_sound(&self) -> Vec<(Vec<f32>, u64)> {
        self.sound.as_ref().map(Heal::take_sound).unwrap_or_default()
    }

    /// Whether captured sound excludes Ferrocast's own monitor playback.
    pub fn excludes_self_audio(&self) -> bool {
        self.sound.as_ref().is_none_or(Heal::excludes_self)
    }

    /// Output device id captured when using legacy device loopback (`None` when process-excluded).
    pub fn loopback_device(&self) -> Option<String> {
        let s = self.sound.as_ref()?;
        if s.excludes_self() { None } else { s.device_id() }
    }

    pub fn frames(&self) -> u64 {
        match &self.live {
            Some(Pictures::Native(w)) => w.frames(),
            Some(Pictures::Ffmpeg(l)) => l.frames(),
            // Sound alone: its pieces are its sign of life, so a source that came back resets
            // the watchdog's waits (else they stayed at a minute; agy's review). Not steady:
            // quiet speakers send nothing, which isn't a stop.
            None => self.sound.as_ref().map_or(0, Heal::received),
        }
    }

    /// Why the capture ended, if it did: its picture, or its sound (the speakers it heard were
    /// unplugged or changed, or the app quit), which is started again with it.
    pub fn ended(&self) -> Option<String> {
        let picture = match self.live.as_ref() {
            Some(Pictures::Native(w)) => w.ended(),
            Some(Pictures::Ffmpeg(l)) => l.ended(),
            None => None,
        };
        picture.or_else(|| self.sound.as_ref()?.ended().map(|e| format!("its sound: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_EX_TOOLWINDOW, WS_POPUP, WS_VISIBLE,
    };
    use windows::core::w;

    /// A visible window of the predefined STATIC class, closed when dropped.
    struct Shown(HWND);
    impl Shown {
        fn new(ex: WINDOW_EX_STYLE, title: windows::core::PCWSTR) -> Shown {
            // SAFETY: a system class, no parent, menu or instance; the handle is ours to destroy.
            let h = unsafe {
                CreateWindowExW(ex, w!("STATIC"), title, WS_POPUP | WS_VISIBLE, 0, 0, 40, 40, None, None, None, None)
            }
            .expect("a test window");
            Shown(h)
        }
        fn listed(&self, list: &[Window]) -> bool {
            list.iter().any(|w| w.id == self.0.0 as usize as u32)
        }
    }
    impl Drop for Shown {
        fn drop(&mut self) {
            // SAFETY: the window this test made, on this thread.
            let _ = unsafe { DestroyWindow(self.0) };
        }
    }

    #[test]
    fn the_window_list_leaves_out_tool_windows() {
        let normal = Shown::new(WINDOW_EX_STYLE(0), w!("ferrocast test window"));
        let tool = Shown::new(WS_EX_TOOLWINDOW, w!("ferrocast test tool window"));
        let (_, picked) = shareable().unwrap();
        assert!(normal.listed(&picked), "a normal window is listed");
        assert!(!tool.listed(&picked), "a tool window (a tray helper, an overlay) isn't");
        // Found again whatever it is, for a source that already names it.
        assert!(tool.listed(&windows()));
    }
}
