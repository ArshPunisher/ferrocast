//! The plugin's own window (Mac): its editor view in a window of the helper's, made and shown
//! on the main thread, which runs AppKit. Closing it hides it; opening it again shows the same
//! view. Windows and Linux: to come (docs/plan/VST.md).

use std::cell::RefCell;
use std::ffi::c_void;

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSBackingStoreType, NSWindow, NSWindowStyleMask};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use vst3::Steinberg::Vst::IEditControllerTrait;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComWrapper};

use super::instance::Instance;

struct Editor {
    window: Retained<NSWindow>,
    view: ComPtr<IPlugView>,
    _frame: ComPtr<IPlugFrame>,
}

thread_local! {
    static EDITOR: RefCell<Option<Editor>> = const { RefCell::new(None) };
}

/// What the plugin calls to be given another size: the window follows.
struct Frame {
    window: Retained<NSWindow>,
}

impl Class for Frame {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for Frame {
    #[allow(unsafe_code)]
    unsafe fn resizeView(&self, view: *mut IPlugView, size: *mut ViewRect) -> tresult {
        if size.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: the plugin's rect, read for the call.
        let r = unsafe { *size };
        self.window.setContentSize(NSSize::new(f64::from(r.right - r.left), f64::from(r.bottom - r.top)));
        // SAFETY: the plugin's own view, told its new size as the SDK asks.
        if let Some(v) = unsafe { vst3::ComRef::from_raw(view) } {
            unsafe { v.onSize(size) };
        }
        kResultOk
    }
}

/// Opens the window (made the first time), in front. Main thread only.
#[allow(unsafe_code)]
pub fn open(inst: &Instance) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;
    let shown = EDITOR.with(|e| {
        e.borrow().as_ref().map(|ed| {
            ed.window.makeKeyAndOrderFront(None);
        })
    });
    let app = NSApplication::sharedApplication(mtm);
    if shown.is_some() {
        app.activate();
        return Ok(());
    }
    let c = inst.controller.as_ref().ok_or("this plugin has no window of its own")?;
    // SAFETY: the SDK's name for the editor view.
    let raw = unsafe { c.createView(c"editor".as_ptr()) };
    // SAFETY: an owned reference, or null.
    let view = unsafe { ComPtr::from_raw(raw) }.ok_or("this plugin has no window of its own")?;
    // SAFETY: a static NUL-ended type name.
    if unsafe { view.isPlatformTypeSupported(c"NSView".as_ptr()) } != kResultTrue {
        return Err("its window isn't made for a Mac".into());
    }
    let mut rect = ViewRect { left: 0, top: 0, right: 400, bottom: 300 };
    // SAFETY: a rect for the answer.
    unsafe { view.getSize(&mut rect) };
    let (w, h) = (f64::from((rect.right - rect.left).max(100)), f64::from((rect.bottom - rect.top).max(60)));
    // SAFETY: a plain query.
    let resizable = unsafe { view.canResize() } == kResultTrue;
    let mut style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable;
    if resizable {
        style |= NSWindowStyleMask::Resizable;
    }
    // SAFETY: a new window on the main thread with a valid rect and style.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h)),
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // Kept when closed, so it can be shown again (and let go with the editor).
    // SAFETY: plain setter, main thread.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str(&inst.name));
    window.center();
    let frame =
        ComWrapper::new(Frame { window: window.clone() }).to_com_ptr::<IPlugFrame>().ok_or("no frame object")?;
    // SAFETY: the frame lives as long as the editor (kept below).
    unsafe { view.setFrame(frame.as_ptr()) };
    let content = window.contentView().ok_or("the window has no view")?;
    let parent = Retained::as_ptr(&content) as *mut c_void;
    // SAFETY: the window's content view, alive while the editor is; the platform type it said yes to.
    if unsafe { view.attached(parent, c"NSView".as_ptr()) } != kResultOk {
        return Err("its window wouldn't open".into());
    }
    window.makeKeyAndOrderFront(None);
    app.activate();
    EDITOR.with(|e| *e.borrow_mut() = Some(Editor { window, view, _frame: frame }));
    Ok(())
}

/// Hides the window (kept for next time). Main thread only.
pub fn close() {
    EDITOR.with(|e| {
        if let Some(ed) = e.borrow().as_ref() {
            ed.window.orderOut(None);
        }
    });
}

/// Lets the editor go before the plugin does. Main thread only.
#[allow(unsafe_code)]
pub fn drop_editor() {
    EDITOR.with(|e| {
        if let Some(ed) = e.borrow_mut().take() {
            // SAFETY: the SDK's order: removed from its parent, then let go.
            unsafe {
                ed.view.removed();
                ed.view.setFrame(std::ptr::null_mut());
            }
            ed.window.orderOut(None);
        }
    });
}
