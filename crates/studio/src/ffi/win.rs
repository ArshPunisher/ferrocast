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
use std::rc::Rc;
use std::sync::Once;
use std::sync::atomic::{AtomicUsize, Ordering};

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
    BeginDeferWindowPos, CS_DBLCLKS, CreateWindowExW, DefWindowProcW, DeferWindowPos, DestroyWindow, EndDeferWindowPos,
    GWLP_USERDATA, GetClientRect, GetWindowLongPtrW, HTCLIENT, HWND_TOP, IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_IBEAM,
    IDC_NO, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, LoadCursorW, MA_NOACTIVATE,
    RegisterClassW, SC_MINIMIZE, SW_HIDE, SW_SHOWNA, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SWP_SHOWWINDOW, SetCursor, SetWindowLongPtrW, SetWindowPos, ShowWindow, WM_CANCELMODE, WM_DPICHANGED,
    WM_ENTERSIZEMOVE, WM_ERASEBKGND, WM_EXITSIZEMOVE, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
    WM_MBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCDESTROY, WM_PAINT,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_SIZING, WM_SYSCOMMAND, WNDCLASSW, WS_CHILD, WS_CLIPSIBLINGS,
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
    input: RefCell<Option<InputFn>>,
    /// 0x00BBGGRR (GDI's COLORREF).
    backdrop: Cell<u32>,
    /// Its top left in the parent's client area, in device pixels.
    origin: Cell<(i32, i32)>,
    /// Its top left in CSS pixels (used to align relative holes to the parent pixel grid).
    origin_css: Cell<(f64, f64)>,
    /// Latest requested client size in device pixels (tracks deferred batch moves accurately).
    size: Cell<(u32, u32)>,
    /// Cached region key so unchanged holes avoid `SetWindowRgn` churn.
    mask_key: RefCell<Option<Option<crate::HoleMaskKey>>>,
    cursor: Cell<*const u16>,
    tracking: Cell<bool>,
}

struct WinBatchOp {
    hwnd: HWND,
    pos: Option<(i32, i32, i32, i32)>,
    visible: Option<bool>,
    region: Option<Option<crate::HoleMaskKey>>,
}

thread_local! {
    static WIN_BATCH: RefCell<Option<Vec<WinBatchOp>>> = const { RefCell::new(None) };
}

fn queue_batch_op(hwnd: HWND, pos: Option<(i32, i32, i32, i32)>, visible: Option<bool>) -> bool {
    WIN_BATCH.with(|b| {
        let mut borrow = b.borrow_mut();
        let Some(ops) = borrow.as_mut() else { return false };
        if let Some(existing) = ops.iter_mut().find(|o| std::ptr::eq(o.hwnd, hwnd)) {
            if pos.is_some() {
                existing.pos = pos;
            }
            if visible.is_some() {
                existing.visible = visible;
            }
        } else {
            ops.push(WinBatchOp { hwnd, pos, visible, region: None });
        }
        true
    })
}

fn queue_batch_rgn(hwnd: HWND, region: Option<crate::HoleMaskKey>) -> bool {
    WIN_BATCH.with(|b| {
        let mut borrow = b.borrow_mut();
        let Some(ops) = borrow.as_mut() else { return false };
        if let Some(existing) = ops.iter_mut().find(|o| std::ptr::eq(o.hwnd, hwnd)) {
            existing.region = Some(region);
        } else {
            ops.push(WinBatchOp { hwnd, pos: None, visible: None, region: Some(region) });
        }
        true
    })
}

fn pending_batch_rgn(hwnd: HWND) -> Option<Option<crate::HoleMaskKey>> {
    WIN_BATCH.with(|b| {
        let borrow = b.borrow();
        let ops = borrow.as_ref()?;
        ops.iter().find(|o| std::ptr::eq(o.hwnd, hwnd)).and_then(|o| o.region.clone())
    })
}

fn drop_batch_ops(hwnd: HWND) {
    WIN_BATCH.with(|b| {
        if let Some(ops) = b.borrow_mut().as_mut() {
            ops.retain(|o| !std::ptr::eq(o.hwnd, hwnd));
        }
    });
}

/// Applies `key` as `hwnd`'s window region (`None` clears the region).
/// Returns `true` iff `SetWindowRgn` succeeded; deletes any created `HRGN` on failure.
///
/// # Safety
/// `hwnd` must be a live window created by this module on the main thread.
unsafe fn apply_window_rgn(hwnd: HWND, key: Option<&crate::HoleMaskKey>) -> bool {
    let Some(key) = key else {
        // SAFETY: `hwnd` is live; passing null clears the custom window region.
        return unsafe { SetWindowRgn(hwnd, std::ptr::null_mut(), 1) } != 0;
    };
    // SAFETY: creates GDI regions for `hwnd`; on `SetWindowRgn` success the OS owns `shown`,
    // and on any failure all locally created GDI region handles are deleted.
    unsafe {
        let shown: HRGN = CreateRectRgn(0, 0, key.width as i32, key.height as i32);
        if shown.is_null() {
            return false;
        }
        for &(hx, hy, hw, hh, d) in &key.holes {
            // CreateRoundRectRgn leaves out its right and bottom edges: one more each.
            let hole = CreateRoundRectRgn(hx, hy, hx + hw + 1, hy + hh + 1, d, d);
            if hole.is_null() {
                DeleteObject(shown);
                return false;
            }
            CombineRgn(shown, shown, hole, RGN_DIFF);
            DeleteObject(hole);
        }
        if SetWindowRgn(hwnd, shown, 1) != 0 {
            true
        } else {
            DeleteObject(shown);
            false
        }
    }
}

/// Runs `f` and commits all child-window moves/resizes/visibility changes in one `DeferWindowPos` batch.
pub fn batch_update<T>(f: impl FnOnce() -> T) -> T {
    let outer = WIN_BATCH.with(|b| b.borrow().is_some());
    if outer {
        return f();
    }
    WIN_BATCH.with(|b| *b.borrow_mut() = Some(Vec::new()));
    struct BatchGuard;
    impl Drop for BatchGuard {
        fn drop(&mut self) {
            let ops = WIN_BATCH.with(|b| b.borrow_mut().take()).unwrap_or_default();
            if ops.is_empty() {
                return;
            }
            // SAFETY: `ops` only contains live child HWNDs created by this module (destroyed ones are pruned).
            unsafe {
                let swp_count = ops.iter().filter(|o| o.pos.is_some() || o.visible.is_some()).count();
                if swp_count > 0 {
                    let mut hdwp = BeginDeferWindowPos(swp_count as i32);
                    if !hdwp.is_null() {
                        for op in &ops {
                            if op.pos.is_none() && op.visible.is_none() {
                                continue;
                            }
                            let (x, y, w, h, mut flags) = match op.pos {
                                Some((x, y, w, h)) => (x, y, w, h, SWP_NOACTIVATE),
                                None => (0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE),
                            };
                            match op.visible {
                                Some(true) => flags |= SWP_SHOWWINDOW,
                                Some(false) => flags |= SWP_HIDEWINDOW,
                                None => {}
                            }
                            hdwp = DeferWindowPos(hdwp, op.hwnd, HWND_TOP, x, y, w, h, flags);
                            if hdwp.is_null() {
                                break;
                            }
                        }
                    }
                    if hdwp.is_null() || EndDeferWindowPos(hdwp) == 0 {
                        for op in &ops {
                            if let Some((x, y, w, h)) = op.pos {
                                SetWindowPos(op.hwnd, HWND_TOP, x, y, w, h, SWP_NOACTIVATE);
                            }
                            if let Some(show) = op.visible {
                                ShowWindow(op.hwnd, if show { SW_SHOWNA } else { SW_HIDE });
                            }
                        }
                    }
                }
                // Apply region updates after child bounds and visibility are committed so
                // `SetWindowRgn(..., bRedraw=TRUE)` never paints against stale dimensions.
                for op in &ops {
                    if let Some(ref next_key) = op.region
                        && apply_window_rgn(op.hwnd, next_key.as_ref())
                        && let Some(state) = (GetWindowLongPtrW(op.hwnd, GWLP_USERDATA) as *const State).as_ref()
                    {
                        *state.mask_key.borrow_mut() = Some(next_key.clone());
                    }
                }
            }
        }
    }
    let _guard = BatchGuard;
    f()
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

/// A box in CSS pixels -> device pixels covering it (x, y, w, h): on whole pixels, rounded
/// outwards, using the shared `device_rect` math.
fn device(x: f64, y: f64, w: f64, h: f64, s: f64) -> (i32, i32, i32, i32) {
    let r = crate::device_rect(x, y, w, h, s);
    (r.x, r.y, r.w as i32, r.h as i32)
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
    let in_batch = WIN_BATCH.with(|b| b.borrow().is_some());
    let style = if in_batch { WS_CHILD | WS_CLIPSIBLINGS } else { WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS };
    // SAFETY: a child of the live window the toolkit handed out, of the class registered above.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            CLASS.as_ptr(),
            std::ptr::null(),
            style,
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
    state.origin_css.set((x, y));
    state.size.set((dw.max(1) as u32, dh.max(1) as u32));
    // SAFETY: the window keeps a pointer to the boxed State, which ChildView owns and frees only
    // after DestroyWindow (whose WM_NCDESTROY clears the pointer). Over the page: on top.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, &*state as *const State as isize);
        SetWindowPos(hwnd, HWND_TOP, dx, dy, dw, dh, SWP_NOACTIVATE);
    }
    if in_batch {
        queue_batch_op(hwnd, None, Some(true));
    }
    Some(ChildView { hwnd, state })
}

impl ChildView {
    pub fn ptr(&self) -> *mut c_void {
        self.hwnd
    }

    /// Its size in device pixels (what libobs draws into).
    pub fn pixels(&self) -> Option<(u32, u32)> {
        let cached = self.state.size.get();
        if cached.0 > 0 && cached.1 > 0 {
            return Some(cached);
        }
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
        self.state.origin_css.set((x, y));
        self.state.origin.set((dx, dy));
        let next_size = (dw.max(1) as u32, dh.max(1) as u32);
        if self.state.size.replace(next_size) != next_size && !matches!(*self.state.mask_key.borrow(), Some(None)) {
            *self.state.mask_key.borrow_mut() = None;
        }
        if !queue_batch_op(self.hwnd, Some((dx, dy, dw, dh)), None) {
            // SAFETY: moves this live window; HWND_TOP keeps it over the webview.
            unsafe { SetWindowPos(self.hwnd, HWND_TOP, dx, dy, dw, dh, SWP_NOACTIVATE) };
        }
    }

    pub fn remove(&self) {
        drop_batch_ops(self.hwnd);
        if let Ok(mut cb) = self.state.input.try_borrow_mut() {
            *cb = None;
        }
        // SAFETY: destroys this module's own window (once: ChildView isn't Clone).
        unsafe { DestroyWindow(self.hwnd) };
    }

    /// The colour under the picture (0xAABBGGRR, as libobs takes it).
    pub fn set_backdrop(&self, abgr: u32) {
        self.state.backdrop.set(abgr & 0x00FF_FFFF);
    }

    /// Shown or hidden: a child window can't be partly see-through over WebView2, so anything
    /// under 5% hides it (the page dims it with `set_shade` instead).
    pub fn set_opacity(&self, opacity: f64) {
        let show = opacity >= 0.05;
        if !queue_batch_op(self.hwnd, None, Some(show)) {
            // SAFETY: shows or hides this live window without activating it.
            unsafe { ShowWindow(self.hwnd, if show { SW_SHOWNA } else { SW_HIDE }) };
        }
    }

    /// Leaves the picture out of `holes` (x, y, w, h and corner radius in CSS pixels from the
    /// view's top left), so the page's menus and panels there show and take clicks.
    pub fn set_holes(&self, holes: &[(f64, f64, f64, f64, f64)]) {
        let Some((pw, ph)) = self.pixels() else { return };
        let s = scale(self.hwnd);
        let origin_css = self.state.origin_css.get();
        let next_key = crate::hole_mask_key_px(origin_css, (pw, ph), holes, s);
        if let Some(pending) = pending_batch_rgn(self.hwnd) {
            if pending == next_key {
                return;
            }
        } else if self.state.mask_key.borrow().as_ref() == Some(&next_key) {
            return;
        }
        if queue_batch_rgn(self.hwnd, next_key.clone()) {
            return;
        }
        // SAFETY: `self.hwnd` is live while `ChildView` exists; `mask_key` updates only on success.
        if unsafe { apply_window_rgn(self.hwnd, next_key.as_ref()) } {
            *self.state.mask_key.borrow_mut() = Some(next_key);
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

static NEXT_SUBCLASS_ID: AtomicUsize = AtomicUsize::new(0x4643_5757);

struct SubclassState {
    hooks: crate::WindowWatchHooks,
    in_size_move: Cell<bool>,
    live_resizing: Cell<bool>,
    removed: Cell<bool>,
}

pub struct WindowWatch {
    hwnd: HWND,
    uid: usize,
    state: Rc<SubclassState>,
}

impl std::fmt::Debug for WindowWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowWatch").field("hwnd", &self.hwnd).field("uid", &self.uid).finish_non_exhaustive()
    }
}

unsafe extern "system" fn window_subclass_proc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
    uid: usize,
    ref_data: usize,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass};
    let ptr = ref_data as *const SubclassState;
    if ptr.is_null() {
        // SAFETY: forwards messages when no subclass state is attached.
        return unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: `ref_data` holds a live `Rc<SubclassState>` until `RemoveWindowSubclass` runs.
    // Incrementing before running callbacks keeps `state` valid even if a hook detaches `WindowWatch`.
    let state = unsafe {
        Rc::increment_strong_count(ptr);
        Rc::from_raw(ptr)
    };
    if state.removed.get() {
        // SAFETY: forwards messages after subclass removal has begun.
        return unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
    }
    match msg {
        WM_ENTERSIZEMOVE => {
            // Pure title-bar moves must keep video live; only enter live-resize hiding on WM_SIZING.
            state.in_size_move.set(true);
            // SAFETY: forwards message to the next window procedure in the subclass chain.
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }
        WM_SIZING => {
            if state.in_size_move.get() && !state.live_resizing.replace(true) {
                (state.hooks.on_live_resize)(true);
            }
            // SAFETY: forwards message to the next window procedure in the subclass chain.
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }
        WM_EXITSIZEMOVE => {
            // SAFETY: let Tao clear its size-move state before notifying the host.
            let res = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
            state.in_size_move.set(false);
            if state.live_resizing.replace(false) {
                (state.hooks.on_live_resize)(false);
            }
            res
        }
        WM_DPICHANGED => {
            // SAFETY: let Tao update scale factor and resize the parent HWND/WebView2 first.
            let res = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
            let dpi = (wparam.0 & 0xffff) as u32;
            if dpi > 0 {
                (state.hooks.on_scale_changed)(f64::from(dpi) / 96.0);
            }
            res
        }
        WM_SYSCOMMAND if (wparam.0 & 0xfff0) == SC_MINIMIZE as usize => {
            // SAFETY: forwards minimize command before clearing active resize state.
            let res = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
            state.in_size_move.set(false);
            state.live_resizing.set(false);
            (state.hooks.on_window_reset)();
            res
        }
        WM_NCDESTROY => {
            state.in_size_move.set(false);
            state.live_resizing.set(false);
            if !state.removed.replace(true) {
                // SAFETY: detaches this subclass as the host window tears down and frees its `Rc` ref.
                unsafe {
                    let _ = RemoveWindowSubclass(hwnd, Some(window_subclass_proc), uid);
                    Rc::decrement_strong_count(ptr);
                }
            }
            // SAFETY: forwards final non-client destroy to the original window procedure chain.
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }
        // SAFETY: forwards all unhandled messages down the subclass chain.
        _ => unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) },
    }
}

impl Drop for WindowWatch {
    fn drop(&mut self) {
        use windows::Win32::Foundation::HWND as WinHWND;
        use windows::Win32::UI::Shell::RemoveWindowSubclass;
        if !self.state.removed.replace(true) {
            // SAFETY: removes our unique `uid` subclass on the live window and releases its `Rc` ref.
            unsafe {
                let _ = RemoveWindowSubclass(WinHWND(self.hwnd), Some(window_subclass_proc), self.uid);
                Rc::decrement_strong_count(Rc::as_ptr(&self.state));
            }
        }
    }
}

pub fn window_in_live_resize(hwnd: *mut c_void) -> bool {
    use windows::Win32::Foundation::HWND as WinHWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GUI_INMOVESIZE, GUITHREADINFO, GetGUIThreadInfo, GetWindowThreadProcessId,
    };
    if hwnd.is_null() {
        return false;
    }
    // SAFETY: queries Win32 GUI state for the thread owning `hwnd`.
    unsafe {
        let target = WinHWND(hwnd);
        let tid = GetWindowThreadProcessId(target, None);
        if tid == 0 {
            return false;
        }
        let mut info = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        GetGUIThreadInfo(tid, &mut info).is_ok() && info.flags.contains(GUI_INMOVESIZE) && info.hwndMoveSize.0 == hwnd
    }
}

pub fn watch_window(hwnd: *mut c_void, hooks: crate::WindowWatchHooks) -> Option<WindowWatch> {
    use windows::Win32::Foundation::HWND as WinHWND;
    use windows::Win32::UI::Shell::SetWindowSubclass;
    if hwnd.is_null() {
        return None;
    }
    let uid = NEXT_SUBCLASS_ID.fetch_add(1, Ordering::Relaxed);
    let state = Rc::new(SubclassState {
        hooks,
        in_size_move: Cell::new(false),
        live_resizing: Cell::new(false),
        removed: Cell::new(false),
    });
    let raw = Rc::into_raw(state.clone());
    // SAFETY: `hwnd` is the main Tauri window's live HWND; the subclass holds `raw` until removed.
    let ok = unsafe { SetWindowSubclass(WinHWND(hwnd), Some(window_subclass_proc), uid, raw as usize) };
    if !ok.as_bool() {
        // SAFETY: installation failed; reclaim the subclass `Rc` ref immediately.
        unsafe { drop(Rc::from_raw(raw)) };
        return None;
    }
    Some(WindowWatch { hwnd, uid, state })
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
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::Once;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, TRUE, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MONITORINFOEXW,
        MonitorFromWindow,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AdjustWindowRectEx, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, GWL_EXSTYLE, GWLP_USERDATA,
        GetClientRect, GetWindowLongPtrW, HTCLIENT, HWND_NOTOPMOST, HWND_TOPMOST, IDC_ARROW, LoadCursorW,
        MONITORINFOF_PRIMARY, PostMessageW, RegisterClassW, SW_SHOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        SWP_NOZORDER, SetCursor, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow, WM_APP,
        WM_CANCELMODE, WM_CLOSE, WM_DPICHANGED, WM_ENTERSIZEMOVE, WM_ERASEBKGND, WM_EXITSIZEMOVE, WM_KEYDOWN,
        WM_LBUTTONDOWN, WM_NCDESTROY, WM_SETCURSOR, WM_SIZE, WM_SIZING, WMSZ_BOTTOM, WMSZ_BOTTOMLEFT, WMSZ_BOTTOMRIGHT,
        WMSZ_LEFT, WMSZ_RIGHT, WMSZ_TOP, WMSZ_TOPLEFT, WMSZ_TOPRIGHT, WNDCLASSW, WS_EX_TOPMOST, WS_OVERLAPPEDWINDOW,
        WS_POPUP, WS_VISIBLE,
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
        /// True while the user is interactively dragging a window border/corner.
        pub live_resize: Box<dyn Fn(bool)>,
        /// The pointer is hidden over the window.
        pub hide_cursor: Cell<bool>,
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
        in_size_move: Cell<bool>,
        live_resizing: Cell<bool>,
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
            // Over the picture the pointer is hidden when asked (OBS's "Hide cursor over projectors").
            WM_SETCURSOR if state.hooks.hide_cursor.get() && (lparam & 0xffff) as u32 == HTCLIENT => {
                // SAFETY: a null cursor hides the pointer for this message; Windows asks again as it moves.
                unsafe { SetCursor(std::ptr::null_mut()) };
                1
            }
            WM_ENTERSIZEMOVE => {
                state.in_size_move.set(true);
                // SAFETY: forwards message to the default window procedure.
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
            WM_EXITSIZEMOVE | WM_CANCELMODE => {
                state.in_size_move.set(false);
                if state.live_resizing.replace(false) {
                    let hooks = state.hooks.clone();
                    (hooks.live_resize)(false);
                }
                // SAFETY: forwards message to the default window procedure.
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
            WM_SIZE => {
                let (w, h) = (lparam as u32 & 0xffff, (lparam as u32 >> 16) & 0xffff);
                (state.hooks.resize)(w.max(1), h.max(1));
                0
            }
            WM_DPICHANGED => {
                if state.full {
                    // Fullscreen projectors stay snapped to their monitor instead of scaling across displays.
                    // SAFETY: queries monitor bounds for this live fullscreen projector window.
                    unsafe {
                        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                        let mut info: MONITORINFO = std::mem::zeroed();
                        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
                        if !mon.is_null() && GetMonitorInfoW(mon, &mut info) != 0 {
                            let r = info.rcMonitor;
                            SetWindowPos(
                                hwnd,
                                std::ptr::null_mut(),
                                r.left,
                                r.top,
                                r.right - r.left,
                                r.bottom - r.top,
                                SWP_NOZORDER | SWP_NOACTIVATE,
                            );
                        }
                    }
                    return 0;
                }
                if lparam != 0 {
                    // SAFETY: Windows passes the suggested RECT in lparam for a top-level DPI change.
                    let r = unsafe { &*(lparam as *const RECT) };
                    // SAFETY: resizes this live projector window; `WM_SIZE` delivers `hooks.resize` if size changed.
                    unsafe {
                        SetWindowPos(
                            hwnd,
                            std::ptr::null_mut(),
                            r.left,
                            r.top,
                            r.right - r.left,
                            r.bottom - r.top,
                            SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                    }
                }
                0
            }
            WM_SIZING if !state.full => {
                if state.in_size_move.get() && !state.live_resizing.replace(true) {
                    let hooks = state.hooks.clone();
                    (hooks.live_resize)(true);
                }
                if state.aspect <= 0.0 {
                    // SAFETY: forwards to the default window procedure when no aspect lock is active.
                    return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
                }
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
            // Ferrocore draws all of it.
            WM_ERASEBKGND => 1,
            WM_NCDESTROY => {
                state.in_size_move.set(false);
                if state.live_resizing.replace(false) {
                    let hooks = state.hooks.clone();
                    (hooks.live_resize)(false);
                }
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
        let state = Box::new(State {
            hooks: Rc::new(hooks),
            aspect,
            full: full.is_some(),
            in_size_move: Cell::new(false),
            live_resizing: Cell::new(false),
        });
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

        /// Hides the pointer over the window, or shows it again.
        pub fn set_hide_cursor(&self, on: bool) {
            // SAFETY: GWLP_USERDATA holds this window's State (boxed in `open`, alive while `Win` is).
            if let Some(state) = unsafe { (GetWindowLongPtrW(self.hwnd, GWLP_USERDATA) as *const State).as_ref() } {
                state.hooks.hide_cursor.set(on);
            }
        }

        pub fn close(&self) {
            // SAFETY: destroys this module's own window (Win isn't Clone; DestroyWindow on a
            // gone one just fails).
            unsafe { DestroyWindow(self.hwnd) };
        }
    }
}
