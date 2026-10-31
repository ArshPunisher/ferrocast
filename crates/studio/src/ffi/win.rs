//! The Studio's picture in the app's window on Windows: a child window over the web page that
//! libobs draws into with D3D11, as the macOS `mac::ChildView` is an NSView over it.
//!
//! On a Mac clicks pass through the picture to the page under it. A child window on Windows
//! can't hand clicks to WebView2 (another process's window), so it takes them itself and gives
//! each to `set_input`'s callback; the app passes them on to the page, which handles picking,
//! moving and resizing sources exactly as with real clicks. The page says which cursor to show
//! (`set_cursor`), as it would over its own stage.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::sync::Once;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CombineRgn, CreateRectRgn, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, EndPaint, FillRect,
    HRGN, PAINTSTRUCT, RGN_DIFF, ScreenToClient, SetWindowRgn,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_CONTROL, VK_MENU, VK_SHIFT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CS_DBLCLKS, CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetClientRect, GetWindowLongPtrW,
    HTCLIENT, HWND_TOP, IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_IBEAM, IDC_NO, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS,
    IDC_SIZENWSE, IDC_SIZEWE, LoadCursorW, MA_NOACTIVATE, RegisterClassW, SW_HIDE, SW_SHOWNA, SWP_NOACTIVATE,
    SetCursor, SetWindowLongPtrW, SetWindowPos, ShowWindow, WM_ERASEBKGND, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCDESTROY, WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WNDCLASSW, WS_CHILD, WS_CLIPSIBLINGS,
    WS_VISIBLE,
};

use crate::PreviewInput as Input;

// From other parts of the Windows API (not worth their crate features): winuser.h's
// WM_MOUSELEAVE and the MK_ button flags of mouse messages.
const WM_MOUSELEAVE: u32 = 0x02A3;
const MK_LBUTTON: u32 = 0x0001;
const MK_RBUTTON: u32 = 0x0002;
const MK_MBUTTON: u32 = 0x0010;

/// Where mouse actions over the picture go.
type InputFn = Box<dyn Fn(Input)>;

/// What the window procedure reads: set when the view is made, dropped after the window goes.
#[derive(Default)]
struct State {
    resize: Cell<Option<(*const super::Api, *mut super::ObsDisplay)>>,
    input: RefCell<Option<InputFn>>,
    /// 0x00BBGGRR (GDI's COLORREF).
    backdrop: Cell<u32>,
    /// Its top left in the parent's client area, in device pixels.
    origin: Cell<(i32, i32)>,
    cursor: Cell<*const u16>,
    tracking: Cell<bool>,
}

/// A child window of the app's window, in the page's coordinates.
pub struct ChildView {
    hwnd: HWND,
    state: Box<State>,
}

impl std::fmt::Debug for ChildView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildView").field("hwnd", &self.hwnd).finish_non_exhaustive()
    }
}

const CLASS: &[u16] = &[
    b'F' as u16,
    b'e' as u16,
    b'r' as u16,
    b'r' as u16,
    b'o' as u16,
    b'c' as u16,
    b'a' as u16,
    b's' as u16,
    b't' as u16,
    b'P' as u16,
    b'r' as u16,
    b'e' as u16,
    b'v' as u16,
    b'i' as u16,
    b'e' as u16,
    b'w' as u16,
    0,
];

/// The window's device pixels per CSS pixel (its monitor's DPI over 96).
fn scale(hwnd: HWND) -> f64 {
    // SAFETY: GetDpiForWindow only reads a window this module made (0 for a gone one).
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 { 1.0 } else { f64::from(dpi) / 96.0 }
}

/// A box in CSS pixels → device pixels covering it (x, y, w, h): on whole pixels, rounded
/// outwards, as the macOS view is (a box on a half pixel left a row undrawn).
fn device(x: f64, y: f64, w: f64, h: f64, s: f64) -> (i32, i32, i32, i32) {
    let (x0, y0) = ((x * s).floor(), (y * s).floor());
    let (x1, y1) = (((x + w.max(1.0)) * s).ceil(), ((y + h.max(1.0)) * s).ceil());
    let c = |v: f64| v.clamp(-32_768.0, 32_767.0) as i32;
    (c(x0), c(y0), c(x1 - x0).max(1), c(y1 - y0).max(1))
}

fn keys() -> (bool, bool, bool) {
    // SAFETY: GetKeyState only reads this thread's keyboard state.
    let down = |vk: u16| unsafe { GetKeyState(i32::from(vk)) } < 0;
    (down(VK_SHIFT), down(VK_CONTROL), down(VK_MENU))
}

fn buttons_of(wparam: WPARAM) -> u16 {
    let w = wparam as u32;
    let mut b = 0;
    if w & MK_LBUTTON != 0 {
        b |= 1;
    }
    if w & MK_RBUTTON != 0 {
        b |= 2;
    }
    if w & MK_MBUTTON != 0 {
        b |= 4;
    }
    b
}

/// The signed x and y packed in a mouse message's LPARAM.
fn point_of(lparam: LPARAM) -> (i32, i32) {
    (i32::from(lparam as u16 as i16), i32::from((lparam >> 16) as u16 as i16))
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: GWLP_USERDATA holds the State this module boxed for this window, set right after
    // CreateWindowExW and cleared (WM_NCDESTROY) before the box goes; null before and after.
    let state = unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const State).as_ref() };
    let Some(state) = state else {
        // SAFETY: the default procedure for messages before the state is set.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    let send = |kind: &'static str, button: i16, buttons: u16, (px, py): (i32, i32), dx: f64, dy: f64| {
        let s = scale(hwnd);
        let (ox, oy) = state.origin.get();
        let (shift, ctrl, alt) = keys();
        let input = Input {
            kind,
            button,
            buttons,
            x: f64::from(ox + px) / s,
            y: f64::from(oy + py) / s,
            dx,
            dy,
            shift,
            ctrl,
            alt,
        };
        if let Ok(cb) = state.input.try_borrow()
            && let Some(cb) = cb.as_ref()
        {
            cb(input);
        }
    };
    match msg {
        WM_MOUSEMOVE => {
            if !state.tracking.get() {
                let mut t = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                // SAFETY: asks for one WM_MOUSELEAVE for this live window; `t` outlives the call.
                if unsafe { TrackMouseEvent(&mut t) } != 0 {
                    state.tracking.set(true);
                }
            }
            send("move", 0, buttons_of(wparam), point_of(lparam), 0.0, 0.0);
            0
        }
        WM_MOUSELEAVE => {
            state.tracking.set(false);
            send("leave", 0, 0, point_of(lparam), 0.0, 0.0);
            0
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
            // Moves and the let-go keep coming here while the button is held, off the window too.
            // SAFETY: captures the mouse for this live window, on its own thread.
            unsafe { SetCapture(hwnd) };
            let button = match msg {
                WM_LBUTTONDOWN => 0,
                WM_MBUTTONDOWN => 1,
                _ => 2,
            };
            send("down", button, buttons_of(wparam), point_of(lparam), 0.0, 0.0);
            0
        }
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP => {
            let buttons = buttons_of(wparam);
            if buttons == 0 {
                // SAFETY: lets go of this thread's mouse capture.
                unsafe { ReleaseCapture() };
            }
            let button = match msg {
                WM_LBUTTONUP => 0,
                WM_MBUTTONUP => 1,
                _ => 2,
            };
            send("up", button, buttons, point_of(lparam), 0.0, 0.0);
            0
        }
        WM_LBUTTONDBLCLK => {
            // Windows sends the second press as a double click: a press, then the double click.
            // SAFETY: as for the button presses above.
            unsafe { SetCapture(hwnd) };
            send("down", 0, buttons_of(wparam), point_of(lparam), 0.0, 0.0);
            send("dblclick", 0, buttons_of(wparam), point_of(lparam), 0.0, 0.0);
            0
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            // The wheel's point is on the screen.
            let (sx, sy) = point_of(lparam);
            let mut p = POINT { x: sx, y: sy };
            // SAFETY: converts a point for this live window; `p` outlives the call.
            unsafe { ScreenToClient(hwnd, &mut p) };
            // 120 a notch; Chromium scrolls 100 CSS pixels a notch, down positive.
            let turn = f64::from((wparam >> 16) as u16 as i16) / 120.0 * 100.0;
            let (dx, dy) = if msg == WM_MOUSEWHEEL { (0.0, -turn) } else { (turn, 0.0) };
            send("wheel", 0, buttons_of(wparam & 0xffff), (p.x, p.y), dx, dy);
            0
        }
        // A click here leaves the keyboard with the page.
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_SETCURSOR if (lparam & 0xffff) as u32 == HTCLIENT => {
            let id = state.cursor.get();
            // SAFETY: a system cursor by its resource id (IDC_ARROW when none was set).
            unsafe { SetCursor(LoadCursorW(std::ptr::null_mut(), if id.is_null() { IDC_ARROW } else { id })) };
            1
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            // The stage's colour where libobs hasn't drawn yet (as the window grows).
            // SAFETY: a standard paint cycle for this window; the brush is made and freed here.
            unsafe {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                let dc = BeginPaint(hwnd, &mut ps);
                if !dc.is_null() {
                    let brush = CreateSolidBrush(state.backdrop.get());
                    FillRect(dc, &ps.rcPaint, brush);
                    DeleteObject(brush);
                }
                EndPaint(hwnd, &ps);
            }
            0
        }
        WM_NCDESTROY => {
            // SAFETY: clears the pointer before ChildView drops the State it points to.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
            // SAFETY: the default procedure finishes destroying the window.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        // SAFETY: everything else goes to the default procedure.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: registers this module's class once, with a procedure that lives forever and a
        // name that's a static NUL-terminated UTF-16 string.
        unsafe {
            let wc = WNDCLASSW {
                style: CS_DBLCLKS,
                lpfnWndProc: Some(proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: GetModuleHandleW(std::ptr::null()),
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: CLASS.as_ptr(),
            };
            RegisterClassW(&wc);
        }
    });
}

/// Adds a child window over `parent` (the app window's HWND) at the box (CSS pixels). None
/// when it couldn't be made.
pub fn child_view(parent: *mut c_void, x: f64, y: f64, w: f64, h: f64) -> Option<ChildView> {
    if parent.is_null() {
        return None;
    }
    register();
    let s = scale(parent);
    let (dx, dy, dw, dh) = device(x, y, w, h, s);
    // SAFETY: a child of the live window the toolkit handed out, of the class registered above.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            CLASS.as_ptr(),
            std::ptr::null(),
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
            dx,
            dy,
            dw,
            dh,
            parent,
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        return None;
    }
    let state = Box::new(State { backdrop: Cell::new(0x000D_0B0A), ..State::default() });
    state.origin.set((dx, dy));
    // SAFETY: the window keeps a pointer to the boxed State, which ChildView owns and frees only
    // after DestroyWindow (whose WM_NCDESTROY clears the pointer). Over the page: on top.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, &*state as *const State as isize);
        SetWindowPos(hwnd, HWND_TOP, dx, dy, dw, dh, SWP_NOACTIVATE);
    }
    Some(ChildView { hwnd, state })
}

impl ChildView {
    pub fn ptr(&self) -> *mut c_void {
        self.hwnd
    }

    /// Its size in device pixels (what libobs draws into).
    pub fn pixels(&self) -> Option<(u32, u32)> {
        let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: reads this live window's client size into `r`.
        if unsafe { GetClientRect(self.hwnd, &mut r) } == 0 {
            return None;
        }
        Some(((r.right - r.left).max(1) as u32, (r.bottom - r.top).max(1) as u32))
    }

    /// Moves and resizes it (CSS pixels), kept over the page.
    pub fn set_frame(&self, x: f64, y: f64, w: f64, h: f64) {
        let (dx, dy, dw, dh) = device(x, y, w, h, scale(self.hwnd));
        self.state.origin.set((dx, dy));
        // SAFETY: moves this live window; HWND_TOP keeps it over the webview.
        unsafe { SetWindowPos(self.hwnd, HWND_TOP, dx, dy, dw, dh, SWP_NOACTIVATE) };
        if let Some((api, display)) = self.state.resize.get() {
            // SAFETY: set by `resize_with` from a live Preview, which calls `remove` (clearing
            // it) before the display and the Api go.
            unsafe { &*api }.obs_display_resize(display, dw as u32, dh as u32);
        }
    }

    pub fn remove(&self) {
        self.state.resize.set(None);
        if let Ok(mut cb) = self.state.input.try_borrow_mut() {
            *cb = None;
        }
        // SAFETY: destroys this module's own window (once: ChildView isn't Clone).
        unsafe { DestroyWindow(self.hwnd) };
    }

    /// Resizes `display` with the view. `remove` lets go.
    pub fn resize_with(&self, api: *const super::Api, display: *mut super::ObsDisplay) {
        self.state.resize.set(Some((api, display)));
    }

    /// The colour under the picture (0xAABBGGRR, as libobs takes it).
    pub fn set_backdrop(&self, abgr: u32) {
        self.state.backdrop.set(abgr & 0x00FF_FFFF);
    }

    /// Shown or hidden: a child window can't be partly see-through over WebView2, so anything
    /// under 5% hides it (the page dims it with `set_shade` instead).
    pub fn set_opacity(&self, opacity: f64) {
        // SAFETY: shows or hides this live window without activating it.
        unsafe { ShowWindow(self.hwnd, if opacity < 0.05 { SW_HIDE } else { SW_SHOWNA }) };
    }

    /// Leaves the picture out of `holes` (x, y, w, h and corner radius in CSS pixels from the
    /// view's top left), so the page's menus and panels there show and take clicks.
    pub fn set_holes(&self, holes: &[(f64, f64, f64, f64, f64)]) {
        let Some((pw, ph)) = self.pixels() else { return };
        let s = scale(self.hwnd);
        // SAFETY: regions made here; the one handed to SetWindowRgn belongs to the system after
        // (not deleted here), the others are deleted.
        unsafe {
            if holes.is_empty() {
                SetWindowRgn(self.hwnd, std::ptr::null_mut(), 1);
                return;
            }
            let shown: HRGN = CreateRectRgn(0, 0, pw as i32, ph as i32);
            for &(x, y, w, h, r) in holes {
                if w <= 0.0 || h <= 0.0 {
                    continue;
                }
                let (hx, hy, hw, hh) = device(x, y, w, h, s);
                let d = (r.max(0.0).min(w / 2.0).min(h / 2.0) * 2.0 * s).round() as i32;
                // CreateRoundRectRgn leaves out its right and bottom edges: one more each.
                let hole = CreateRoundRectRgn(hx, hy, hx + hw + 1, hy + hh + 1, d, d);
                CombineRgn(shown, shown, hole, RGN_DIFF);
                DeleteObject(hole);
            }
            SetWindowRgn(self.hwnd, shown, 1);
        }
    }

    /// Mouse actions over it go to `cb` (on this thread, as they happen).
    pub fn set_input(&self, cb: InputFn) {
        if let Ok(mut c) = self.state.input.try_borrow_mut() {
            *c = Some(cb);
        }
    }

    /// The cursor over it, by its CSS name (what the page shows over its stage there).
    pub fn set_cursor(&self, css: &str) {
        let id = match css {
            "pointer" => IDC_HAND,
            "move" | "grab" | "grabbing" | "all-scroll" => IDC_SIZEALL,
            "nwse-resize" | "nw-resize" | "se-resize" => IDC_SIZENWSE,
            "nesw-resize" | "ne-resize" | "sw-resize" => IDC_SIZENESW,
            "ns-resize" | "n-resize" | "s-resize" | "row-resize" => IDC_SIZENS,
            "ew-resize" | "e-resize" | "w-resize" | "col-resize" => IDC_SIZEWE,
            "crosshair" => IDC_CROSS,
            "text" => IDC_IBEAM,
            "not-allowed" | "no-drop" => IDC_NO,
            _ => IDC_ARROW,
        };
        self.state.cursor.set(id);
        if self.state.tracking.get() {
            // SAFETY: shows it now, the mouse being over this window (a system cursor by id).
            unsafe { SetCursor(LoadCursorW(std::ptr::null_mut(), id)) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boxes_cover_whole_device_pixels() {
        assert_eq!(device(10.5, 20.25, 100.0, 50.0, 1.5), (15, 30, 151, 76));
        assert_eq!(device(0.0, 0.0, 0.0, 0.0, 1.0), (0, 0, 1, 1));
    }
}

/// Projector windows on Windows (the macOS ones are `ffi::projector_window`): titled and
/// resizable, or borderless over a whole display, with the same hooks.
pub mod projector {
    use std::rc::Rc;
    use std::sync::Once;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, TRUE, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AdjustWindowRectEx, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, GWL_EXSTYLE, GWLP_USERDATA,
        GetClientRect, GetWindowLongPtrW, HWND_NOTOPMOST, HWND_TOPMOST, IDC_ARROW, LoadCursorW, MONITORINFOF_PRIMARY,
        PostMessageW, RegisterClassW, SW_SHOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetForegroundWindow,
        SetWindowLongPtrW, SetWindowPos, ShowWindow, WM_APP, WM_CLOSE, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN,
        WM_NCDESTROY, WM_SIZE, WM_SIZING, WMSZ_BOTTOM, WMSZ_BOTTOMLEFT, WMSZ_BOTTOMRIGHT, WMSZ_LEFT, WMSZ_RIGHT,
        WMSZ_TOP, WMSZ_TOPLEFT, WMSZ_TOPRIGHT, WNDCLASSW, WS_EX_TOPMOST, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE,
    };

    use super::point_of;

    /// What a projector window tells its owner.
    pub struct Hooks {
        /// A click at (x, y) in a view `w`×`h` big (device pixels), from its top left.
        pub click: Box<dyn Fn(f64, f64, f64, f64)>,
        /// Closed (its close button, or Esc): the owner drops it.
        pub close: Box<dyn Fn()>,
        /// The view's new size in pixels.
        pub resize: Box<dyn Fn(u32, u32)>,
    }

    /// Posted to itself so `close` runs after the message that asked for it, never inside it
    /// (the owner destroys the window from there).
    const WM_CLOSE_LATER: u32 = WM_APP + 1;

    const CLASS: &[u16] = &[
        b'F' as u16,
        b'e' as u16,
        b'r' as u16,
        b'r' as u16,
        b'o' as u16,
        b'c' as u16,
        b'a' as u16,
        b's' as u16,
        b't' as u16,
        b'P' as u16,
        b'r' as u16,
        b'o' as u16,
        b'j' as u16,
        b'e' as u16,
        b'c' as u16,
        b't' as u16,
        b'o' as u16,
        b'r' as u16,
        0,
    ];

    struct State {
        hooks: Rc<Hooks>,
        /// Width over height of the picture (a windowed projector keeps it as it's resized).
        aspect: f64,
        full: bool,
    }

    unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // SAFETY: GWLP_USERDATA holds the State boxed in `open` for this window, cleared at
        // WM_NCDESTROY before `Win` drops the box; null before it's set.
        let state = unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const State).as_ref() };
        let Some(state) = state else {
            // SAFETY: the default procedure for messages before the state is set.
            return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
        };
        match msg {
            WM_LBUTTONDOWN => {
                let (x, y) = point_of(lparam);
                let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
                // SAFETY: reads this live window's client size into `r`.
                unsafe { GetClientRect(hwnd, &mut r) };
                (state.hooks.click)(f64::from(x), f64::from(y), f64::from(r.right), f64::from(r.bottom));
                0
            }
            WM_KEYDOWN if wparam == usize::from(VK_ESCAPE) => {
                // SAFETY: posts to this live window's own queue.
                unsafe { PostMessageW(hwnd, WM_CLOSE_LATER, 0, 0) };
                0
            }
            WM_CLOSE => {
                // The close button asks the owner to drop the projector (which closes it).
                // SAFETY: as above.
                unsafe { PostMessageW(hwnd, WM_CLOSE_LATER, 0, 0) };
                0
            }
            WM_CLOSE_LATER => {
                // The owner drops the projector from here, which destroys this window and its
                // State: the hooks are held here until the call is over (as macOS's `later`).
                let hooks = state.hooks.clone();
                (hooks.close)();
                0
            }
            WM_SIZE => {
                let (w, h) = (lparam as u32 & 0xffff, (lparam as u32 >> 16) & 0xffff);
                (state.hooks.resize)(w.max(1), h.max(1));
                0
            }
            WM_SIZING if !state.full && state.aspect > 0.0 => {
                // Keeps the picture's shape: the edge dragged leads, the other follows.
                // SAFETY: lparam is the RECT Windows passes with WM_SIZING, ours to change.
                let r = unsafe { &mut *(lparam as *mut RECT) };
                let mut frame = RECT { left: 0, top: 0, right: 0, bottom: 0 };
                // SAFETY: the frame round an empty client area for this window's styles.
                unsafe {
                    let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
                    AdjustWindowRectEx(&mut frame, WS_OVERLAPPEDWINDOW, 0, ex);
                }
                let (fw, fh) = (frame.right - frame.left, frame.bottom - frame.top);
                let (cw, ch) = (r.right - r.left - fw, r.bottom - r.top - fh);
                match wparam as u32 {
                    WMSZ_TOP | WMSZ_BOTTOM => r.right = r.left + fw + (f64::from(ch) * state.aspect).round() as i32,
                    WMSZ_LEFT | WMSZ_RIGHT | WMSZ_BOTTOMLEFT | WMSZ_BOTTOMRIGHT => {
                        r.bottom = r.top + fh + (f64::from(cw) / state.aspect).round() as i32;
                    }
                    WMSZ_TOPLEFT | WMSZ_TOPRIGHT => {
                        r.top = r.bottom - fh - (f64::from(cw) / state.aspect).round() as i32
                    }
                    _ => {}
                }
                TRUE as LRESULT
            }
            // libobs draws all of it.
            WM_ERASEBKGND => 1,
            WM_NCDESTROY => {
                // SAFETY: clears the pointer before Win drops the State it points to.
                unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
                // SAFETY: the default procedure finishes destroying the window.
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
            // SAFETY: everything else goes to the default procedure.
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    fn register() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            // SAFETY: registers this class once, with a procedure that lives forever and a
            // static NUL-terminated UTF-16 name.
            unsafe {
                let wc = WNDCLASSW {
                    style: 0,
                    lpfnWndProc: Some(proc),
                    cbClsExtra: 0,
                    cbWndExtra: 0,
                    hInstance: GetModuleHandleW(std::ptr::null()),
                    hIcon: std::ptr::null_mut(),
                    hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
                    hbrBackground: std::ptr::null_mut(),
                    lpszMenuName: std::ptr::null(),
                    lpszClassName: CLASS.as_ptr(),
                };
                RegisterClassW(&wc);
            }
        });
    }

    /// Each display: its name, its box on the desktop (device pixels) and whether it's the main one.
    fn monitors() -> Vec<(String, RECT, bool)> {
        unsafe extern "system" fn each(m: HMONITOR, _: HDC, _: *mut RECT, out: LPARAM) -> i32 {
            // SAFETY: `out` is the Vec passed below, alive for the whole enumeration.
            let out = unsafe { &mut *(out as *mut Vec<(String, RECT, bool)>) };
            // SAFETY: a zeroed MONITORINFOEXW with its size set, filled in by Windows.
            let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            // SAFETY: as above; MONITORINFOEXW starts with MONITORINFO.
            if unsafe { GetMonitorInfoW(m, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) } != 0 {
                let n = info.szDevice.iter().position(|c| *c == 0).unwrap_or(info.szDevice.len());
                let device = String::from_utf16_lossy(&info.szDevice[..n]);
                let main = info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0;
                out.push((device, info.monitorInfo.rcMonitor, main));
            }
            TRUE
        }
        let mut out: Vec<(String, RECT, bool)> = Vec::new();
        // SAFETY: enumerates the displays into `out`, which outlives the call.
        unsafe {
            EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(each), &mut out as *mut _ as LPARAM)
        };
        // The main display first, as macOS lists them.
        out.sort_by_key(|(_, _, main)| !main);
        out
    }

    /// The displays, the main one first: (name, width, height in pixels).
    pub fn screens() -> Vec<(String, f64, f64)> {
        monitors()
            .into_iter()
            .enumerate()
            .map(|(i, (_, r, _))| {
                let (w, h) = (r.right - r.left, r.bottom - r.top);
                (format!("Display {}", i + 1), f64::from(w), f64::from(h))
            })
            .collect()
    }

    /// An open projector window.
    pub struct Win {
        hwnd: HWND,
        full: bool,
        _state: Box<State>,
    }

    impl std::fmt::Debug for Win {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Win").field("hwnd", &self.hwnd).finish_non_exhaustive()
        }
    }

    /// Opens one: full screen on display `screen` (borderless, over the taskbar), or a window
    /// the shape `aspect` (width / height). None for no such display.
    pub fn open(title: &str, aspect: f64, screen: Option<usize>, hooks: Hooks) -> Option<Win> {
        register();
        let full = match screen {
            Some(i) => Some(monitors().get(i)?.1),
            None => None,
        };
        let aspect = if aspect > 0.0 { aspect } else { 16.0 / 9.0 };
        let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        let (style, ex, x, y, w, h) = match full {
            Some(r) => (WS_POPUP | WS_VISIBLE, WS_EX_TOPMOST, r.left, r.top, r.right - r.left, r.bottom - r.top),
            None => {
                let mut r = RECT { left: 0, top: 0, right: 960, bottom: (960.0 / aspect).round() as i32 };
                // SAFETY: grows `r` by the frame of a normal window.
                unsafe { AdjustWindowRectEx(&mut r, WS_OVERLAPPEDWINDOW, 0, 0) };
                // Windows places (and cascades) each new one.
                (WS_OVERLAPPEDWINDOW, 0, CW_USEDEFAULT, CW_USEDEFAULT, r.right - r.left, r.bottom - r.top)
            }
        };
        // SAFETY: a top-level window of the class registered above, with a NUL-terminated title.
        let hwnd = unsafe {
            CreateWindowExW(
                ex,
                CLASS.as_ptr(),
                title.as_ptr(),
                style,
                x,
                y,
                w,
                h,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            return None;
        }
        let state = Box::new(State { hooks: Rc::new(hooks), aspect, full: full.is_some() });
        // SAFETY: the window keeps a pointer to the boxed State, which Win owns and frees only
        // after DestroyWindow (whose WM_NCDESTROY clears it). Then shown and given the keys (Esc).
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, &*state as *const State as isize);
            ShowWindow(hwnd, SW_SHOW);
            SetForegroundWindow(hwnd);
        }
        Some(Win { hwnd, full: full.is_some(), _state: state })
    }

    impl Win {
        pub fn view_ptr(&self) -> *mut std::ffi::c_void {
            self.hwnd
        }

        pub fn number(&self) -> isize {
            self.hwnd as isize
        }

        /// The view's size in pixels now.
        pub fn pixels(&self) -> (u32, u32) {
            let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            // SAFETY: reads this live window's client size into `r`.
            unsafe { GetClientRect(self.hwnd, &mut r) };
            ((r.right - r.left).max(1) as u32, (r.bottom - r.top).max(1) as u32)
        }

        /// Keeps a windowed projector above other apps' windows (OBS's "Always on top"); a
        /// full-screen one stays above everything anyway.
        pub fn set_on_top(&self, on: bool) {
            if !self.full {
                let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
                // SAFETY: changes only this live window's z-order.
                unsafe { SetWindowPos(self.hwnd, after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE) };
            }
        }

        pub fn close(&self) {
            // SAFETY: destroys this module's own window (Win isn't Clone; DestroyWindow on a
            // gone one just fails).
            unsafe { DestroyWindow(self.hwnd) };
        }
    }
}
