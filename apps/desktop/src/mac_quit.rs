//! macOS quitting, the parts tao/tauri don't give: (1) a Quit that comes from outside the app,
//! the Dock's menu or an AppleEvent (`NSApp terminate:`), reaches the app delegate's
//! `applicationShouldTerminate:`, and tao's delegate has none, so it quit at once with a
//! recording running (S59, 21 Nov). Added here to that class at runtime: it asks like every
//! other quit path. (2) The question is an `NSAlert` run modally, so Return picks the default
//! button (Cancel) and Esc picks Cancel, as a Mac user expects; the system alert tauri's dialog
//! plugin gives (`CFUserNotification`) answers Esc and ignores Return.
//!
//! Plain calls into the Objective-C runtime (it is always there; no crate needed).

#![allow(unsafe_code)]
// Every call here is a plain Objective-C runtime call on the main thread; the module's header says so.
#![allow(clippy::undocumented_unsafe_blocks)]

use std::ffi::{CString, c_char, c_void};
use std::sync::OnceLock;
use tauri::AppHandle;

type Id = *mut c_void;
type Sel = *mut c_void;
type Class = *mut c_void;

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Class;
    fn object_getClass(obj: Id) -> Class;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_msgSend();
    fn class_addMethod(cls: Class, name: Sel, imp: *const c_void, types: *const c_char) -> u8;
    fn objc_allocateClassPair(superclass: Class, name: *const c_char, extra_bytes: usize) -> Class;
    fn objc_registerClassPair(cls: Class);
    fn object_setClass(obj: Id, cls: Class) -> Class;
}

/// `NSApplicationTerminateReply`.
const TERMINATE_CANCEL: usize = 0;
const TERMINATE_NOW: usize = 1;
const TERMINATE_LATER: usize = 2;
/// `NSAlertFirstButtonReturn`.
const FIRST_BUTTON: isize = 1000;

static APP: OnceLock<AppHandle> = OnceLock::new();

fn c(s: &str) -> CString {
    CString::new(s).unwrap_or_default()
}

unsafe fn class(name: &str) -> Class {
    unsafe { objc_getClass(c(name).as_ptr()) }
}

unsafe fn sel(name: &str) -> Sel {
    unsafe { sel_registerName(c(name).as_ptr()) }
}

/// `[obj name]`.
unsafe fn send0(obj: Id, name: &str) -> Id {
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel) -> Id = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f(obj, sel(name))
    }
}

/// `[obj name: arg]` with an object argument.
unsafe fn send1(obj: Id, name: &str, a: Id) -> Id {
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, Id) -> Id = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f(obj, sel(name), a)
    }
}

/// `[obj name: arg]` with an integer argument (an enum, a BOOL).
unsafe fn send_int(obj: Id, name: &str, a: isize) -> Id {
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, isize) -> Id = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f(obj, sel(name), a)
    }
}

/// An `NSString` from text.
unsafe fn nsstring(text: &str) -> Id {
    unsafe {
        let s = c(text);
        let f: unsafe extern "C" fn(Id, Sel, *const c_char) -> Id =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f(class("NSString"), sel("stringWithUTF8String:"), s.as_ptr())
    }
}

/// What the delegate says to a terminate request (the pure part, tested).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    /// Quit now: the app is already leaving, or nothing needs asking.
    Now,
    /// Not now, and ask nothing more: a question is already open.
    Cancel,
    /// Later: ask, and tell the system what the answer was.
    Later,
}

pub(crate) fn terminate_answer(leaving: bool, asking: bool) -> Answer {
    if leaving {
        Answer::Now
    } else if asking {
        Answer::Cancel
    } else {
        Answer::Later
    }
}

/// The delegate's `applicationShouldTerminate:`.
extern "C" fn should_terminate(_this: Id, _cmd: Sel, _sender: Id) -> usize {
    let Some(app) = APP.get() else { return TERMINATE_NOW };
    match crate::quit::terminate_requested(app) {
        Answer::Now => TERMINATE_NOW,
        Answer::Cancel => TERMINATE_CANCEL,
        Answer::Later => TERMINATE_LATER,
    }
}

/// Adds `applicationShouldTerminate:` to the app delegate's class (tao's), once the app runs.
pub(crate) fn install(app: &AppHandle) {
    let _ = APP.set(app.clone());
    // SAFETY: plain Objective-C runtime calls on the main thread (`RunEvent::Ready`).
    unsafe {
        let ns_app = send0(class("NSApplication"), "sharedApplication");
        let delegate = send0(ns_app, "delegate");
        if delegate.is_null() {
            tracing::warn!("no app delegate: a Quit from the Dock can't be asked about");
            return;
        }
        let cls = object_getClass(delegate);
        let added = class_addMethod(
            cls,
            sel("applicationShouldTerminate:"),
            should_terminate as extern "C" fn(Id, Sel, Id) -> usize as *const c_void,
            c("Q@:@").as_ptr(),
        );
        if added == 0 {
            tracing::warn!(
                "the app delegate already answers applicationShouldTerminate: a Quit from the Dock isn't asked about by us"
            );
        }
    }
}

/// Tells the system the answer to a terminate request that was put off (main thread).
pub(crate) fn reply_terminate(go: bool) {
    // SAFETY: as above; `replyToApplicationShouldTerminate:` takes a BOOL.
    unsafe {
        let ns_app = send0(class("NSApplication"), "sharedApplication");
        send_int(ns_app, "replyToApplicationShouldTerminate:", isize::from(go));
    }
}

/// The question's words: the buttons, Cancel first (the default: Return; and Esc).
pub(crate) fn buttons(restart: bool) -> [&'static str; 2] {
    ["Cancel", if restart { "Restart" } else { "Quit" }]
}

/// `NSButton` that also answers Esc (and Return, Enter) by clicking itself. AppKit gives one key
/// equivalent to a button: the default button's is Return, and a button titled "Cancel" would get
/// Esc, but not both. The Cancel button is both here.
extern "C" fn cancel_key(this: Id, _cmd: Sel, event: Id) -> u8 {
    // SAFETY: `event` is the key-down `NSEvent` AppKit is asking about; `keyCode` is a plain getter.
    let code = unsafe {
        let f: unsafe extern "C" fn(Id, Sel) -> u16 = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f(event, sel("keyCode"))
    };
    if matches!(code, KEY_ESC | KEY_RETURN | KEY_ENTER) {
        // SAFETY: a click on the button that was asked.
        unsafe { send1(this, "performClick:", std::ptr::null_mut()) };
        1
    } else {
        0
    }
}

/// Virtual key codes.
const KEY_RETURN: u16 = 36;
const KEY_ENTER: u16 = 76;
const KEY_ESC: u16 = 53;

/// The `NSButton` subclass of [`cancel_key`], made once.
unsafe fn cancel_button_class() -> Class {
    unsafe {
        let name = c("FerrocastCancelButton");
        let existing = objc_getClass(name.as_ptr());
        if !existing.is_null() {
            return existing;
        }
        let cls = objc_allocateClassPair(class("NSButton"), name.as_ptr(), 0);
        if cls.is_null() {
            return std::ptr::null_mut();
        }
        class_addMethod(
            cls,
            sel("performKeyEquivalent:"),
            cancel_key as extern "C" fn(Id, Sel, Id) -> u8 as *const c_void,
            c("c@:@").as_ptr(),
        );
        objc_registerClassPair(cls);
        cls
    }
}

/// Asks with an `NSAlert`, modally, on the main thread → whether the second button (the
/// action) was chosen. Cancel is the first button: the default, so Return picks it, and the
/// one named "Cancel", so Esc picks it.
pub(crate) fn ask_modal(title: &str, text: &str, restart: bool) -> bool {
    // SAFETY: Objective-C runtime calls, on the main thread (the caller runs it there).
    unsafe {
        let ns_app = send0(class("NSApplication"), "sharedApplication");
        send_int(ns_app, "activateIgnoringOtherApps:", 1);
        let alert = send0(send0(class("NSAlert"), "alloc"), "init");
        send1(alert, "setMessageText:", nsstring(title));
        send1(alert, "setInformativeText:", nsstring(text));
        send_int(alert, "setAlertStyle:", 0); // NSAlertStyleWarning
        let mut first = std::ptr::null_mut();
        for (i, b) in buttons(restart).into_iter().enumerate() {
            let button = send1(alert, "addButtonWithTitle:", nsstring(b));
            if i == 0 {
                first = button;
            }
        }
        // AppKit leaves a first button titled "Cancel" without Return (Esc stays on it): the
        // window's default button is set to it, so Return picks Cancel too.
        if !first.is_null() {
            send1(send0(alert, "window"), "setDefaultButtonCell:", send0(first, "cell"));
            // …and Esc: the default button's key equivalent is Return, so Esc is answered by
            // the button itself.
            let cls = cancel_button_class();
            if !cls.is_null() {
                object_setClass(first, cls);
            }
        }
        let f: unsafe extern "C" fn(Id, Sel) -> isize = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let answer = f(alert, sel("runModal"));
        send0(alert, "release");
        answer != FIRST_BUTTON
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quit_from_outside_is_asked_once_and_never_while_leaving() {
        assert_eq!(terminate_answer(false, false), Answer::Later);
        // A second Quit while the question is open: refused, no second question.
        assert_eq!(terminate_answer(false, true), Answer::Cancel);
        // Already leaving (confirmed, or nothing was on): let it through.
        assert_eq!(terminate_answer(true, false), Answer::Now);
        assert_eq!(terminate_answer(true, true), Answer::Now);
    }

    #[test]
    fn cancel_is_the_first_button_and_a_restart_says_restart() {
        assert_eq!(buttons(false), ["Cancel", "Quit"]);
        assert_eq!(buttons(true), ["Cancel", "Restart"]);
    }
}
