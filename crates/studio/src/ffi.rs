//! The windows and system pieces the Studio draws and listens through (our engine does the
//! picture and sound): the preview's view and projector windows, web pages, live captions,
//! faces, and the person mask. Native calls (AppKit, WebKit, Speech, Vision, Win32): every
//! unsafe block says why it is sound.
#![allow(unsafe_code)]

/// The colours of sources' outlines, picked by item id (`id % 8`); the page's list shows the
/// same: orange, sky, lime, pink, amber, violet, teal, blue.
pub const FRAME_COLORS: [(f32, f32, f32); 8] = [
    (1.0, 0.353, 0.122),
    (0.2, 0.831, 1.0),
    (0.639, 0.902, 0.208),
    (0.957, 0.447, 0.714),
    (0.98, 0.8, 0.082),
    (0.655, 0.545, 0.98),
    (0.176, 0.831, 0.749),
    (0.376, 0.647, 0.98),
];

/// Live captions with Apple's Speech recognizer (plan item 7).
#[cfg(target_os = "macos")]
pub(crate) mod speech;

/// A sound's audio for any listener (Whisper captions).
pub(crate) mod tap;

/// Faces in a picture with Apple's Vision (plan item 8, smart vertical).
#[cfg(target_os = "macos")]
pub(crate) mod vision;

/// Robust Video Matting through tract: the person mask where Vision isn't (Remove background on
/// Windows).
#[cfg(any(windows, feature = "onnx-matte"))]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) mod matte;

/// The Studio's picture in the app's window on Windows (the macOS one is `mac::ChildView`).
#[cfg(windows)]
pub(crate) mod win;
#[cfg(windows)]
pub(crate) use win::projector as projector_window;

/// Web pages as sources (macOS, WebKit).
#[cfg(target_os = "macos")]
pub(crate) mod web;

#[cfg(target_os = "macos")]
pub(crate) mod mac {
    use std::ffi::c_void;

    use objc2::encode::{Encoding, RefEncode};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
    use objc2_app_kit::{NSAutoresizingMaskOptions, NSResponder, NSView};
    use objc2_foundation::{NSObject, NSPoint, NSRect, NSSize};

    /// CoreGraphics' path (`CGPathRef`), only passed along.
    #[repr(C)]
    struct CGPath {
        _opaque: [u8; 0],
    }

    // SAFETY: CGPathRef is `struct CGPath *`, which the Objective-C runtime encodes as below.
    unsafe impl RefEncode for CGPath {
        const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("CGPath", &[]));
    }

    /// CoreGraphics' colour (`CGColorRef`), only passed along.
    #[repr(C)]
    struct CGColor {
        _opaque: [u8; 0],
    }

    // SAFETY: CGColorRef is `struct CGColor *`, encoded by the runtime as below.
    unsafe impl RefEncode for CGColor {
        const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("CGColor", &[]));
    }

    // Linked for CALayer (the mask layer is made by name).
    #[link(name = "QuartzCore", kind = "framework")]
    unsafe extern "C" {}

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPathCreateWithRoundedRect(rect: NSRect, rx: f64, ry: f64, m: *const c_void) -> *mut CGPath;
        fn CGPathRelease(path: *mut CGPath);
        fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
        fn CGColorSpaceRelease(space: *mut c_void);
        fn CGBitmapContextCreate(
            data: *mut c_void,
            width: usize,
            height: usize,
            bits_per_component: usize,
            bytes_per_row: usize,
            space: *mut c_void,
            info: u32,
        ) -> *mut c_void;
        fn CGBitmapContextCreateImage(ctx: *mut c_void) -> *mut c_void;
        fn CGContextRelease(ctx: *mut c_void);
        fn CGContextScaleCTM(ctx: *mut c_void, sx: f64, sy: f64);
        fn CGContextSetRGBFillColor(ctx: *mut c_void, r: f64, g: f64, b: f64, a: f64);
        fn CGContextFillRect(ctx: *mut c_void, rect: NSRect);
        fn CGContextSetBlendMode(ctx: *mut c_void, mode: i32);
        fn CGContextAddPath(ctx: *mut c_void, path: *const CGPath);
        fn CGContextFillPath(ctx: *mut c_void);
        fn CGImageRelease(image: *mut c_void);
        fn CGColorCreateSRGB(r: f64, g: f64, b: f64, a: f64) -> *mut CGColor;
        fn CGColorRelease(color: *mut CGColor);
    }

    /// The view's own state (none: the picture is drawn by our engine's preview thread).
    #[derive(Debug, Default)]
    pub struct PassIvars;

    define_class!(
        // SAFETY: NSView allows subclassing; PassView's ivars are plain Cells (no Drop needed).
        #[unsafe(super(NSView, NSResponder, NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "FerrocastPreviewView"]
        #[ivars = PassIvars]
        struct PassView;

        impl PassView {
            /// Resized (by the page, or by AppKit as the window is resized): the picture is
            /// redrawn at the new size at once, as OBS's preview does, instead of a frame or
            /// two later stretched.
            /// The window jumped in size (full screen, zoom): hidden at once, as the page can't
            /// say where it goes until the change settles (the main thread is busy with the
            /// animation), and a view left at the old size spills over the panels. The page
            /// shows it again where things ended up. A drag's small steps leave it alone: the
            /// page shows a still of it then.
            #[unsafe(method(resizeWithOldSuperviewSize:))]
            fn resize_with_old(&self, old: NSSize) {
                // SAFETY: NSView's own resizeWithOldSuperviewSize: with AppKit's argument.
                let _: () = unsafe { msg_send![super(self), resizeWithOldSuperviewSize: old] };
                // SAFETY: on the main thread (AppKit calls this there); only reads the tree.
                if let Some(p) = unsafe { self.superview() } {
                    let now = p.bounds().size;
                    if (now.width - old.width).abs() > 60.0 || (now.height - old.height).abs() > 60.0 {
                        self.setAlphaValue(0.0);
                    }
                }
            }

            /// Clicks go through to the page under it, which handles picking, moving and
            /// resizing sources (the picture is drawn here, the page only listens).
            #[unsafe(method(hitTest:))]
            fn hit_test(&self, _point: NSPoint) -> *mut NSView {
                std::ptr::null_mut()
            }
        }
    );

    /// A child view of the window's content, in top-left coordinates like the web page.
    pub struct ChildView {
        view: Retained<NSView>,
    }

    impl std::fmt::Debug for ChildView {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("ChildView").field("view", &self.view).finish_non_exhaustive()
        }
    }

    /// Where the web page sits inside `parent`: the largest view in it (the webview; a preview
    /// is always smaller). The page can start below the window's title bar, so its box, not
    /// the parent's, is what page coordinates count from (2.1: the preview sat a title bar
    /// too high and covered the buttons above it).
    fn page(parent: &NSView, except: Option<&NSView>) -> NSRect {
        let mut best = parent.bounds();
        let mut area = 0.0;
        for v in parent.subviews().iter() {
            if except.is_some_and(|e| std::ptr::eq(e, &*v)) {
                continue;
            }
            let f = v.frame();
            let a = f.size.width * f.size.height;
            if a > area {
                area = a;
                best = f;
            }
        }
        best
    }

    fn frame(parent: &NSView, except: Option<&NSView>, x: f64, y: f64, w: f64, h: f64, vh: f64) -> NSRect {
        let p = page(parent, except);
        // The page's visible top: `vh` up from the webview's bottom (it can be lower than the
        // webview's top when the page draws under the title bar).
        let vh = if vh > 0.0 && vh <= p.size.height { vh } else { p.size.height };
        // AppKit counts from the bottom unless the view is flipped; the page counts from the top.
        let top = if parent.isFlipped() { p.origin.y + (p.size.height - vh) + y } else { p.origin.y + vh - y - h };
        // On whole device pixels, covering the box: a frame on a half pixel (the page's boxes
        // often are) left a row of the GL surface undrawn, a black line along the stage.
        let s = parent.window().map(|w| w.backingScaleFactor()).filter(|s| *s > 0.0).unwrap_or(1.0);
        let (x0, y0) = (((p.origin.x + x) * s).floor() / s, (top * s).floor() / s);
        let (x1, y1) = (((p.origin.x + x + w.max(1.0)) * s).ceil() / s, ((top + h.max(1.0)) * s).ceil() / s);
        NSRect::new(NSPoint::new(x0, y0), NSSize::new(x1 - x0, y1 - y0))
    }

    /// Adds a view over `parent` (an `NSView *` the window toolkit handed out) at the rectangle.
    /// None off the main thread (AppKit allows views only there).
    pub fn child_view(parent: *mut c_void, x: f64, y: f64, w: f64, h: f64, vh: f64) -> Option<ChildView> {
        let mtm = MainThreadMarker::new()?;
        if parent.is_null() {
            return None;
        }
        // SAFETY: `parent` is the live NSView from the window toolkit's raw handle (Tauri/tao),
        // used on the main thread while that window exists.
        let parent: &NSView = unsafe { &*(parent as *const NSView) };
        let pass = PassView::alloc(mtm).set_ivars(PassIvars);
        // SAFETY: NSView's designated initialiser on a freshly allocated PassView.
        let view: Retained<PassView> =
            unsafe { msg_send![super(pass), initWithFrame: frame(parent, None, x, y, w, h, vh)] };
        let view: Retained<NSView> = Retained::into_super(view);
        view.setWantsLayer(true);
        // Kept the same distance from the top when the window's height changes: unflipped,
        // AppKit would keep it from the bottom and slide it over the page above (2.1 test).
        if !parent.isFlipped() {
            view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinYMargin);
        }
        parent.addSubview(&view);
        Some(ChildView { view })
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
    }

    /// macOS lets this app record the screen (as it was when the app started: macOS applies a
    /// change only after a restart).
    pub fn screen_access() -> bool {
        // SAFETY: a plain CoreGraphics query with no arguments, callable from any thread.
        unsafe { CGPreflightScreenCaptureAccess() }
    }

    /// Asks for screen recording: macOS shows its prompt (once) and lists the app in System
    /// Settings. True if it's already allowed.
    pub fn request_screen_access() -> bool {
        // SAFETY: as above; it only shows the system's prompt.
        unsafe { CGRequestScreenCaptureAccess() }
    }

    impl ChildView {
        pub fn ptr(&self) -> *mut c_void {
            Retained::as_ptr(&self.view) as *mut c_void
        }

        /// Its size in device pixels (what libobs draws into). None off the main thread.
        pub fn pixels(&self) -> Option<(u32, u32)> {
            MainThreadMarker::new()?;
            let f = self.view.frame();
            let s = self.view.window().map(|w| w.backingScaleFactor()).unwrap_or(1.0);
            let px = |v: f64| (v * s).round().clamp(1.0, 16_384.0) as u32;
            Some((px(f.size.width), px(f.size.height)))
        }

        /// Moves and resizes it (the page's box moved). Main thread only; ignored elsewhere.
        pub fn set_frame(&self, x: f64, y: f64, w: f64, h: f64, vh: f64) {
            if MainThreadMarker::new().is_none() {
                return;
            }
            // SAFETY: on the main thread (checked above); `superview` only reads the view tree.
            if let Some(parent) = unsafe { self.view.superview() } {
                self.view.setFrame(frame(&parent, Some(&self.view), x, y, w, h, vh));
            }
        }

        pub fn remove(&self) {
            if MainThreadMarker::new().is_some() {
                self.view.removeFromSuperview();
            }
        }

        /// The view's own colour under the picture (0xAABBGGRR, as libobs takes it), shown
        /// where the picture isn't drawn yet. Main thread only; ignored elsewhere.
        pub fn set_backdrop(&self, abgr: u32) {
            if MainThreadMarker::new().is_none() {
                return;
            }
            // SAFETY: `layer` on a layer-backed NSView (wantsLayer is set when it's made).
            let layer: Option<Retained<AnyObject>> = unsafe { msg_send![&*self.view, layer] };
            let Some(layer) = layer else { return };
            let c = |shift: u32| f64::from((abgr >> shift) & 0xff) / 255.0;
            // SAFETY: a new colour handed to the layer (which keeps its own reference), then
            // released here.
            unsafe {
                let color = CGColorCreateSRGB(c(0), c(8), c(16), 1.0);
                if color.is_null() {
                    return;
                }
                let _: () = msg_send![&*layer, setBackgroundColor: color];
                CGColorRelease(color);
            }
        }

        /// How much of the view shows, 0 to 1. Main thread only; ignored elsewhere.
        pub fn set_opacity(&self, opacity: f64) {
            if MainThreadMarker::new().is_some() {
                self.view.setAlphaValue(opacity.clamp(0.0, 1.0));
            }
        }

        /// Leaves the picture out of `holes` (x, y, w, h and corner radius in points from the
        /// view's top left) so the page's menus and panels there show through it; none: all
        /// of it shows. Main thread only; ignored elsewhere.
        ///
        /// The mask is drawn as a picture rather than an even-odd path: holes that overlap (a
        /// sub-menu over its menu, a list over a dialog) stay holes, and rounded corners show
        /// the page's rounded panel, not a square cut of what's under it.
        pub fn set_holes(&self, holes: &[(f64, f64, f64, f64, f64)]) {
            if MainThreadMarker::new().is_none() {
                return;
            }
            // SAFETY: `layer` on a layer-backed NSView (wantsLayer is set when it's made).
            let layer: Option<Retained<AnyObject>> = unsafe { msg_send![&*self.view, layer] };
            let Some(layer) = layer else { return };
            let b = self.view.bounds();
            let inside: Vec<(NSRect, f64)> = holes
                .iter()
                .filter_map(|&(x, y, w, h, rad)| {
                    // The whole box, drawn past the view's edge where it reaches over it: only
                    // its own corners are round (rounding the part inside cut round notches
                    // where a dialog crosses the stage's edge).
                    let inside = x < b.size.width && y < b.size.height && x + w > 0.0 && y + h > 0.0;
                    (inside && w > 0.0 && h > 0.0).then(|| {
                        // The layer counts up from the bottom like its view (not flipped).
                        let y0 = if self.view.isFlipped() { y } else { b.size.height - y - h };
                        let rad = rad.max(0.0).min(w / 2.0).min(h / 2.0);
                        (NSRect::new(NSPoint::new(x, y0), NSSize::new(w, h)), rad)
                    })
                })
                .collect();
            let none: Option<&AnyObject> = None;
            if inside.is_empty() || b.size.width < 1.0 || b.size.height < 1.0 {
                // SAFETY: CALayer's `mask` property takes nil.
                let _: () = unsafe { msg_send![&*layer, setMask: none] };
                return;
            }
            let scale: f64 = self.view.window().map(|w| w.backingScaleFactor()).unwrap_or(2.0);
            let (pw, ph) = ((b.size.width * scale).ceil() as usize, (b.size.height * scale).ceil() as usize);
            let Some(class) = AnyClass::get(c"CALayer") else { return };
            // SAFETY: `+[CALayer layer]` makes a new layer (QuartzCore is linked above).
            let mask: Option<Retained<AnyObject>> = unsafe { msg_send![class, layer] };
            let Some(mask) = mask else { return };
            // SAFETY: a fresh RGBA bitmap context of the view's size in pixels, filled opaque,
            // then each hole cleared; its image is handed to the layer (which keeps its own
            // reference) and everything made here is released here.
            unsafe {
                let space = CGColorSpaceCreateDeviceRGB();
                // kCGImageAlphaPremultipliedLast
                let ctx = CGBitmapContextCreate(std::ptr::null_mut(), pw, ph, 8, 0, space, 1);
                CGColorSpaceRelease(space);
                if ctx.is_null() {
                    return;
                }
                CGContextScaleCTM(ctx, scale, scale);
                CGContextSetRGBFillColor(ctx, 0.0, 0.0, 0.0, 1.0);
                CGContextFillRect(ctx, NSRect::new(NSPoint::new(0.0, 0.0), b.size));
                // kCGBlendModeClear: what's drawn now becomes see-through.
                CGContextSetBlendMode(ctx, 16);
                for (r, rad) in &inside {
                    let path = CGPathCreateWithRoundedRect(*r, *rad, *rad, std::ptr::null());
                    if path.is_null() {
                        continue;
                    }
                    CGContextAddPath(ctx, path);
                    CGContextFillPath(ctx);
                    CGPathRelease(path);
                }
                let image = CGBitmapContextCreateImage(ctx);
                CGContextRelease(ctx);
                if image.is_null() {
                    return;
                }
                let frame = NSRect::new(NSPoint::new(0.0, 0.0), b.size);
                let _: () = msg_send![&*mask, setFrame: frame];
                let _: () = msg_send![&*mask, setContentsScale: scale];
                let _: () = msg_send![&*mask, setContents: image as *mut AnyObject];
                let _: () = msg_send![&*layer, setMask: &*mask];
                CGImageRelease(image);
            }
        }
    }
}

/// Projector windows (macOS): titled and resizable, or borderless over a whole display.
#[cfg(target_os = "macos")]
pub(crate) mod projector_window {
    use std::rc::Rc;

    use dispatch2::DispatchQueue;
    use objc2::rc::Retained;
    use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
    use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
    use objc2_app_kit::{
        NSBackingStoreType, NSEvent, NSResponder, NSScreen, NSView, NSWindow, NSWindowCollectionBehavior,
        NSWindowDelegate, NSWindowStyleMask,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

    /// What a projector window tells its owner.
    pub struct Hooks {
        /// A click at (x, y) in a view `w`×`h` points big, from its top left.
        pub click: Box<dyn Fn(f64, f64, f64, f64)>,
        /// Closed (its close button, or Esc full screen): the owner drops it.
        pub close: Box<dyn Fn()>,
        /// The view's new size in pixels.
        pub resize: Box<dyn Fn(u32, u32)>,
    }

    /// Carries main-thread values to the main queue, where alone they're used.
    struct OnMain<T>(T);
    // SAFETY: made on the main thread and only used in closures the main queue runs.
    unsafe impl<T> Send for OnMain<T> {}

    /// Runs `f` on the main queue after the current event (never inside AppKit's callback, so
    /// the window can go away safely).
    fn later(hooks: &Rc<Hooks>, f: fn(&Hooks)) {
        let job = OnMain(hooks.clone());
        DispatchQueue::main().exec_async(move || {
            // The whole wrapper moves in (a closure would otherwise take just its field).
            let job = job;
            f(&job.0);
        });
    }

    define_class!(
        // SAFETY: NSView allows subclassing; ProjectorView doesn't implement Drop.
        #[unsafe(super(NSView, NSResponder, NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "FerrocastProjectorView"]
        #[ivars = Rc<Hooks>]
        struct ProjectorView;

        impl ProjectorView {
            #[unsafe(method(acceptsFirstResponder))]
            fn accepts_first_responder(&self) -> bool {
                true
            }

            #[unsafe(method(acceptsFirstMouse:))]
            fn accepts_first_mouse(&self, _e: Option<&NSEvent>) -> bool {
                true
            }

            #[unsafe(method(mouseDown:))]
            fn mouse_down(&self, e: &NSEvent) {
                let p = self.convertPoint_fromView(e.locationInWindow(), None);
                let b = self.bounds();
                // Unflipped: AppKit counts up from the bottom.
                (self.ivars().click)(p.x, b.size.height - p.y, b.size.width, b.size.height);
            }

            #[unsafe(method(keyDown:))]
            fn key_down(&self, e: &NSEvent) {
                // Esc leaves a full-screen projector (OBS's key for it).
                if e.keyCode() == 53 {
                    later(self.ivars(), |h| (h.close)());
                }
            }

            #[unsafe(method(setFrameSize:))]
            fn set_frame_size(&self, size: NSSize) {
                // SAFETY: NSView's own setFrameSize: on this view, with the size AppKit gave.
                let _: () = unsafe { msg_send![super(self), setFrameSize: size] };
                let px = self.convertSizeToBacking(size);
                (self.ivars().resize)(px.width.round().max(1.0) as u32, px.height.round().max(1.0) as u32);
            }
        }
    );

    define_class!(
        // SAFETY: NSWindow allows subclassing; ProjectorWin adds no ivars, no Drop.
        #[unsafe(super(NSWindow, NSResponder, NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "FerrocastProjectorWindow"]
        struct ProjectorWin;

        impl ProjectorWin {
            /// A borderless window takes keys too (Esc), like OBS's full-screen projector.
            #[unsafe(method(canBecomeKeyWindow))]
            fn can_become_key(&self) -> bool {
                true
            }
        }
    );

    define_class!(
        // SAFETY: NSObject has no subclassing requirements; Closer doesn't implement Drop.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "FerrocastProjectorCloser"]
        #[ivars = Rc<Hooks>]
        struct Closer;

        unsafe impl NSObjectProtocol for Closer {}

        unsafe impl NSWindowDelegate for Closer {
            /// The close button asks the owner to drop the projector (which closes it).
            #[unsafe(method(windowShouldClose:))]
            fn window_should_close(&self, _w: &NSWindow) -> bool {
                later(self.ivars(), |h| (h.close)());
                false
            }
        }
    );

    thread_local! {
        /// Where the next projector window's top left goes (main thread).
        static CASCADE: std::cell::Cell<Option<NSPoint>> = const { std::cell::Cell::new(None) };
    }

    /// An open projector window.
    pub struct Win {
        window: Retained<ProjectorWin>,
        view: Retained<ProjectorView>,
        _closer: Retained<Closer>,
    }

    /// The displays, the main one first as macOS lists them: (name, width, height in points).
    pub fn screens() -> Vec<(String, f64, f64)> {
        let Some(mtm) = MainThreadMarker::new() else { return Vec::new() };
        NSScreen::screens(mtm)
            .iter()
            .map(|s| {
                let f = s.frame();
                (s.localizedName().to_string(), f.size.width, f.size.height)
            })
            .collect()
    }

    /// Opens one: full screen on display `screen` (borderless, over the menu bar), or a window
    /// the shape `aspect` (width / height). None off the main thread or for no such display.
    pub fn open(title: &str, aspect: f64, screen: Option<usize>, hooks: Hooks) -> Option<Win> {
        let mtm = MainThreadMarker::new()?;
        let hooks = Rc::new(hooks);
        let full = match screen {
            Some(i) => {
                let all = NSScreen::screens(mtm);
                if i >= all.count() {
                    return None;
                }
                Some(all.objectAtIndex(i).frame())
            }
            None => None,
        };
        let (rect, style) = match full {
            Some(f) => (f, NSWindowStyleMask::Borderless),
            None => {
                let w = 960.0;
                let h = (w / if aspect > 0.0 { aspect } else { 16.0 / 9.0 }).round();
                (
                    NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h)),
                    NSWindowStyleMask::Titled
                        | NSWindowStyleMask::Closable
                        | NSWindowStyleMask::Miniaturizable
                        | NSWindowStyleMask::Resizable,
                )
            }
        };
        let alloc = ProjectorWin::alloc(mtm).set_ivars(());
        // SAFETY: NSWindow's designated initialiser on a freshly allocated ProjectorWin, on the
        // main thread, with valid arguments.
        let window: Retained<ProjectorWin> = unsafe {
            msg_send![super(alloc), initWithContentRect: rect, styleMask: style, backing: NSBackingStoreType::Buffered, defer: false]
        };
        // SAFETY: the window is ours (its Retained releases it), not released on close.
        unsafe { window.setReleasedWhenClosed(false) };
        let content = NSRect::new(NSPoint::new(0.0, 0.0), rect.size);
        let alloc = ProjectorView::alloc(mtm).set_ivars(hooks.clone());
        // SAFETY: NSView's designated initialiser on a freshly allocated ProjectorView.
        let view: Retained<ProjectorView> = unsafe { msg_send![super(alloc), initWithFrame: content] };
        view.setWantsLayer(true);
        window.setContentView(Some(&view));
        let closer: Retained<Closer> = {
            let alloc = Closer::alloc(mtm).set_ivars(hooks);
            // SAFETY: NSObject's init on a freshly allocated object.
            unsafe { msg_send![super(alloc), init] }
        };
        window.setDelegate(Some(ProtocolObject::from_ref(&*closer)));
        window.setTitle(&NSString::from_str(title));
        if full.is_some() {
            // Over the menu bar and the Dock, on every Space, as OBS's full-screen projector.
            window.setLevel(25);
            window.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces | NSWindowCollectionBehavior::FullScreenAuxiliary,
            );
            window.setFrame_display(rect, true);
        } else {
            window.setContentAspectRatio(NSSize::new(aspect.max(0.1), 1.0));
            // Each new window steps down and right from the last, as macOS cascades them.
            CASCADE.with(|at| match at.get() {
                Some(p) => at.set(Some(window.cascadeTopLeftFromPoint(p))),
                None => {
                    window.center();
                    let f = window.frame();
                    at.set(Some(window.cascadeTopLeftFromPoint(NSPoint::new(f.origin.x, f.origin.y + f.size.height))));
                }
            });
        }
        window.makeKeyAndOrderFront(None);
        window.makeFirstResponder(Some(&view));
        Some(Win { window, view, _closer: closer })
    }

    impl std::fmt::Debug for Win {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Win").finish_non_exhaustive()
        }
    }

    impl Win {
        pub fn view_ptr(&self) -> *mut std::ffi::c_void {
            Retained::as_ptr(&self.view) as *mut std::ffi::c_void
        }

        pub fn number(&self) -> isize {
            self.window.windowNumber()
        }

        /// The view's size in pixels now.
        pub fn pixels(&self) -> (u32, u32) {
            let px = self.view.convertSizeToBacking(self.view.bounds().size);
            (px.width.round().max(1.0) as u32, px.height.round().max(1.0) as u32)
        }

        /// Keeps a windowed projector above other apps' windows (OBS's "Always on top"); a
        /// full-screen one stays above everything anyway.
        pub fn set_on_top(&self, on: bool) {
            if MainThreadMarker::new().is_some() && self.window.level() != 25 {
                // NSFloatingWindowLevel is 3; NSNormalWindowLevel 0.
                self.window.setLevel(if on { 3 } else { 0 });
            }
        }

        pub fn close(&self) {
            if MainThreadMarker::new().is_some() {
                self.window.setDelegate(None);
                self.window.orderOut(None);
                self.window.close();
            }
        }
    }
}
