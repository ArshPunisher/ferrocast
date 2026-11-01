//! Ferrocast's Web page source (macOS; the Browser source on macOS's own WebKit, LESSONS R-78):
//! a page rendered by a WKWebView in an off-screen window, snapshotted into our engine's fed
//! source, its sound (Web Audio, audio and video elements) into the mixer. Settings use OBS's
//! Browser source keys (url, width, height, css, fps), so a collection moves over.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchTime};
use std::sync::Weak;

use base64::Engine as _;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSBackingStoreType, NSImage, NSScreen, NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask};
use objc2_foundation::{NSError, NSPoint, NSRect, NSSize, NSString, NSURL, NSURLRequest};
use objc2_web_kit::{
    WKAudiovisualMediaTypes, WKInactiveSchedulingPolicy, WKScriptMessage, WKScriptMessageHandler,
    WKSnapshotConfiguration, WKUserContentController, WKUserScript, WKUserScriptInjectionTime, WKWebView,
    WKWebViewConfiguration,
};


/// What every thread may touch.
struct Shared {
    /// The source is visible somewhere (preview or program): snapshots only then.
    shown: AtomicBool,
    /// The source still exists; the snapshot loop stops when it doesn't.
    alive: AtomicBool,
    w: AtomicU32,
    h: AtomicU32,
    fps: AtomicU32,
    /// Our engine's source the page's picture and sound go to.
    out: Mutex<Option<ferrocast_av::engine::Feed>>,
}

/// The off-screen window and its web view. Main thread only.
struct Page {
    window: Retained<NSWindow>,
    /// Holds the web view; scaled to fit the screen while the page is being clicked.
    holder: Retained<NSView>,
    view: Retained<WKWebView>,
    css: String,
    url: String,
    size: (u32, u32),
    /// On screen, to click and type in ("Interact"), with this title.
    interact: Option<String>,
    /// Kept alive with the page: WebKit and AppKit hold these weakly or until removed.
    tap: Retained<AudioTap>,
    closer: Option<Retained<WindowCloser>>,
}

/// The name pages post their sound to.
const AUDIO_HANDLER: &str = "ferrocastAudio";

/// Runs first in every page and frame: sends what the page plays (Web Audio and audio or
/// video elements) to Ferrocast instead of the speakers, as 16-bit stereo chunks, so it
/// goes into the stream through the mixer like OBS's "Control audio via OBS".
const AUDIO_SCRIPT: &str = r#"(function () {
  if (window.__ferrocastAudio) return
  window.__ferrocastAudio = true
  var h = window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.ferrocastAudio
  var AC = window.AudioContext || window.webkitAudioContext
  if (!h || !AC) return
  var connect = AudioNode.prototype.connect
  var disconnect = AudioNode.prototype.disconnect
  var taps = new WeakMap()
  function send(rate, l, r) {
var n = l.length, out = new Int16Array(n * 2), loud = false
for (var i = 0; i < n; i++) {
  var a = l[i], b = r[i]
  if (a !== 0 || b !== 0) loud = true
  a = a < -1 ? -1 : a > 1 ? 1 : a
  b = b < -1 ? -1 : b > 1 ? 1 : b
  out[2 * i] = a * 32767
  out[2 * i + 1] = b * 32767
}
if (!loud) return
var bytes = new Uint8Array(out.buffer), s = ''
for (var j = 0; j < bytes.length; j += 0x8000)
  s += String.fromCharCode.apply(null, bytes.subarray(j, j + 0x8000))
h.postMessage(rate + ':' + btoa(s))
  }
  function tap(ctx) {
var t = taps.get(ctx)
if (t) return t
t = ctx.createScriptProcessor(2048, 2, 2)
var mute = ctx.createGain()
mute.gain.value = 0
connect.call(t, mute)
connect.call(mute, ctx.destination)
t.onaudioprocess = function (e) {
  var b = e.inputBuffer, l = b.getChannelData(0)
  send(ctx.sampleRate, l, b.numberOfChannels > 1 ? b.getChannelData(1) : l)
}
taps.set(ctx, t)
return t
  }
  AudioNode.prototype.connect = function (dest) {
if (dest instanceof AudioDestinationNode) {
  var args = Array.prototype.slice.call(arguments)
  args[0] = tap(this.context)
  connect.apply(this, args)
  return dest
}
return connect.apply(this, arguments)
  }
  AudioNode.prototype.disconnect = function (dest) {
if (dest instanceof AudioDestinationNode) {
  var args = Array.prototype.slice.call(arguments)
  args[0] = tap(this.context)
  return disconnect.apply(this, args)
}
return disconnect.apply(this, arguments)
  }
  var shared = null, routed = new WeakSet()
  function route(el) {
if (!(el instanceof HTMLMediaElement) || routed.has(el)) return
routed.add(el)
try {
  shared = shared || new AC()
  shared.createMediaElementSource(el).connect(shared.destination)
  if (shared.state === 'suspended') shared.resume()
} catch (e) {}
  }
  var play = HTMLMediaElement.prototype.play
  HTMLMediaElement.prototype.play = function () {
route(this)
return play.apply(this, arguments)
  }
  document.addEventListener('play', function (e) { route(e.target) }, true)
})()"#;

/// One chunk from the page ("48000:<base64 of 16-bit stereo>") into the source's sound.
fn feed(shared: &Shared, msg: &str) {
    let Some((rate, b64)) = msg.split_once(':') else { return };
    let Ok(rate) = rate.parse::<u32>() else { return };
    if !(8_000..=192_000).contains(&rate) {
        return;
    }
    let Ok(pcm) = base64::engine::general_purpose::STANDARD.decode(b64) else { return };
    let frames = (pcm.len() / 4) as u32;
    if frames == 0 {
        return;
    }
    // Into our engine: as floats, stamped when the chunk began.
    let Some(out) = shared.out.lock().ok().and_then(|o| o.clone()) else { return };
    let samples: Vec<f32> =
        pcm.as_chunks::<2>().0.iter().map(|b| f32::from(i16::from_le_bytes(*b)) / 32768.0).collect();
    let span = u64::from(frames) * 1_000_000_000 / u64::from(rate);
    out.sound(samples, rate, ferrocast_av::engine::now_ns().saturating_sub(span));
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; AudioTap doesn't implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FerrocastWebAudioTap"]
    #[ivars = Arc<Shared>]
    struct AudioTap;

    unsafe impl NSObjectProtocol for AudioTap {}

    unsafe impl WKScriptMessageHandler for AudioTap {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive(&self, _ucc: &WKUserContentController, message: &WKScriptMessage) {
            // SAFETY: the message is live for this call; its body is a JS string here.
            let body = unsafe { message.body() };
            if let Ok(s) = body.downcast::<NSString>() {
                feed(self.ivars(), &s.to_string());
            }
        }
    }
);

impl AudioTap {
    fn new(mtm: MainThreadMarker, shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(shared);
        // SAFETY: NSObject's init on a freshly allocated object.
        unsafe { msg_send![super(this), init] }
    }
}

type PageSlot = Arc<Mutex<Option<OnMain<Page>>>>;

define_class!(
    // SAFETY: NSObject has no subclassing requirements; WindowCloser doesn't implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FerrocastWebWindowCloser"]
    #[ivars = Weak<Mutex<Option<OnMain<Page>>>>]
    struct WindowCloser;

    unsafe impl NSObjectProtocol for WindowCloser {}

    unsafe impl NSWindowDelegate for WindowCloser {
        /// The close button puts the page back off-screen instead of closing it.
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _w: &NSWindow) -> bool {
            if let Some(page) = self.ivars().upgrade() {
                let job = OnMain(page);
                DispatchQueue::main().exec_async(move || {
                    let OnMain(page) = job;
                    if let Ok(mut p) = page.lock()
                        && let Some(OnMain(p)) = p.as_mut()
                    {
                        place(p, None);
                    }
                });
            }
            false
        }
    }
);

impl WindowCloser {
    fn new(mtm: MainThreadMarker, page: &PageSlot) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Arc::downgrade(page));
        // SAFETY: NSObject's init on a freshly allocated object.
        unsafe { msg_send![super(this), init] }
    }
}

/// Off-screen at full size, or (with a title) on screen to click, scaled to fit.
fn place(p: &mut Page, interact: Option<String>) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let (w, h) = (f64::from(p.size.0), f64::from(p.size.1));
    let full = NSSize::new(w, h);
    p.view.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), full));
    match &interact {
        Some(title) => {
            let fit = NSScreen::mainScreen(mtm).map(|s| s.visibleFrame().size).unwrap_or(full);
            let k = (fit.width * 0.9 / w).min(fit.height * 0.85 / h).min(1.0);
            let shown = NSSize::new((w * k).round(), (h * k).round());
            p.window.setStyleMask(
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable,
            );
            p.window.setTitle(&NSString::from_str(title));
            p.window.setContentSize(shown);
            p.holder.setFrameSize(shown);
            p.holder.setBoundsSize(full);
            p.window.center();
            p.window.makeKeyAndOrderFront(None);
        }
        None => {
            p.window.setStyleMask(NSWindowStyleMask::Borderless);
            p.window.setContentSize(full);
            p.holder.setFrameSize(full);
            p.holder.setBoundsSize(full);
            p.window.setFrameOrigin(NSPoint::new(-20_000.0, -20_000.0));
            p.window.orderBack(None);
        }
    }
    p.interact = interact;
}

/// Carries main-thread objects to the main queue; they're only used there.
struct OnMain<T>(T);
// SAFETY: the wrapped value is created on the main thread and only used in closures run on
// the main queue (DispatchQueue::main), never touched elsewhere.
unsafe impl<T> Send for OnMain<T> {}

/// The page's CSS goes in as a style element once the document loads.
fn css_script(css: &str) -> String {
    let quoted = serde_json::to_string(css).unwrap_or_else(|_| "\"\"".into());
    format!(
        "(function(){{var s=document.getElementById('ferrocast-css')||document.createElement('style');s.id='ferrocast-css';s.textContent={quoted};(document.head||document.documentElement).appendChild(s);}})();"
    )
}

fn open(
    mtm: MainThreadMarker,
    slot: &PageSlot,
    shared: &Arc<Shared>,
    url: &str,
    w: u32,
    h: u32,
    css: &str,
) -> Option<Page> {
    let rect = NSRect::new(NSPoint::new(-20_000.0, -20_000.0), NSSize::new(f64::from(w), f64::from(h)));
    // SAFETY: AppKit window creation on the main thread (mtm) with valid arguments.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: as above; a window we own is released by its Retained, not on close.
    unsafe { window.setReleasedWhenClosed(false) };
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(f64::from(w), f64::from(h)));
    // SAFETY: WebKit objects made and configured on the main thread; each optional call is
    // made only where the object answers to it (respondsToSelector:).
    let view = unsafe {
        let cfg = WKWebViewConfiguration::new(mtm);
        // Sound plays without a click (it goes to the mixer, not the speakers).
        cfg.setMediaTypesRequiringUserActionForPlayback(WKAudiovisualMediaTypes::None);
        // The page keeps running though its window is off-screen: macOS 14+ says so in
        // public (inactiveSchedulingPolicy = none) …
        let prefs = cfg.preferences();
        let has: bool = msg_send![&*prefs, respondsToSelector: objc2::sel!(setInactiveSchedulingPolicy:)];
        if has {
            prefs.setInactiveSchedulingPolicy(WKInactiveSchedulingPolicy::None);
        }
        let view = WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &cfg);
        // … and WebKit's own switch covers macOS 13 (animations froze off-screen, 2.1 test).
        let sel = objc2::sel!(_setWindowOcclusionDetectionEnabled:);
        let has: bool = msg_send![&*view, respondsToSelector: sel];
        if has {
            let _: () = msg_send![&*view, _setWindowOcclusionDetectionEnabled: false];
        }
        view
    };
    // A clear page background (WKWebView's private-but-stable key, as OBS-like hosts use).
    // SAFETY: KVC on a live WKWebView on the main thread; "drawsBackground" is a BOOL key.
    unsafe {
        let no: Retained<AnyObject> = msg_send![objc2::class!(NSNumber), numberWithBool: false];
        let key = NSString::from_str("drawsBackground");
        let _: () = msg_send![&*view, setValue: &*no, forKey: &*key];
    }
    let holder = NSView::initWithFrame(NSView::alloc(mtm), frame);
    holder.addSubview(&view);
    window.setContentView(Some(&holder));
    let tap = AudioTap::new(mtm, shared.clone());
    let closer = WindowCloser::new(mtm, slot);
    window.setDelegate(Some(ProtocolObject::from_ref(&*closer)));
    window.orderBack(None);
    let mut page = Page {
        window,
        holder,
        view,
        css: css.to_string(),
        url: String::new(),
        size: (w, h),
        interact: None,
        tap,
        closer: Some(closer),
    };
    // SAFETY: registering a message handler on the live view's controller, main thread.
    unsafe {
        page.view
            .configuration()
            .userContentController()
            .addScriptMessageHandler_name(ProtocolObject::from_ref(&*page.tap), &NSString::from_str(AUDIO_HANDLER));
    }
    load(&mut page, url, css);
    Some(page)
}

/// The page's CSS, injected at the end of every document it loads (replaces the last).
fn set_css(view: &WKWebView, css: &str) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    // SAFETY: WebKit configuration calls on the live view, main thread.
    unsafe {
        let ucc = view.configuration().userContentController();
        ucc.removeAllUserScripts();
        let audio = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
            WKUserScript::alloc(mtm),
            &NSString::from_str(AUDIO_SCRIPT),
            WKUserScriptInjectionTime::AtDocumentStart,
            false,
        );
        ucc.addUserScript(&audio);
        if !css.trim().is_empty() {
            let src = NSString::from_str(&css_script(css));
            let script = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                WKUserScript::alloc(mtm),
                &src,
                WKUserScriptInjectionTime::AtDocumentEnd,
                true,
            );
            ucc.addUserScript(&script);
        }
    }
}

fn load(page: &mut Page, url: &str, css: &str) {
    if page.css != css || page.url.is_empty() {
        set_css(&page.view, css);
    }
    page.css = css.to_string();
    page.url = url.to_string();
    let url = if url.trim().is_empty() { "about:blank" } else { url.trim() };
    let Some(u) = NSURL::URLWithString(&NSString::from_str(url)) else { return };
    let req = NSURLRequest::requestWithURL(&u);
    // SAFETY: a valid request on the live web view, main thread.
    unsafe {
        let _ = page.view.loadRequest(&req);
    }
}

fn resize(page: &mut Page, w: u32, h: u32) {
    if page.size == (w, h) {
        return;
    }
    page.size = (w, h);
    let at = page.interact.clone();
    place(page, at);
}

/// Something to do with a page: put it on screen to click (with the window's title), take
/// it back, or load it again.
pub(crate) enum Do {
    Interact(String),
    Stop,
    Reload,
}

/// Copies a snapshot into the frame: drawn into a BGRA buffer of the source's size, then
/// un-premultiplied (OBS's default draw takes straight alpha).
fn copy(img: &NSImage, shared: &Shared) {
    let (w, h) = (shared.w.load(Ordering::Relaxed), shared.h.load(Ordering::Relaxed));
    let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
    // SAFETY: CoreGraphics C calls with a buffer of exactly w*h*4 bytes and stride w*4; the
    // CGImage comes from a live NSImage during this call and is only borrowed.
    unsafe {
        let cg: *mut c_void = msg_send![img, CGImageForProposedRect: std::ptr::null_mut::<NSRect>(), context: std::ptr::null_mut::<AnyObject>(), hints: std::ptr::null_mut::<AnyObject>()];
        if cg.is_null() {
            return;
        }
        let cs = cg::CGColorSpaceCreateDeviceRGB();
        let ctx = cg::CGBitmapContextCreate(
            buf.as_mut_ptr().cast(),
            w as usize,
            h as usize,
            8,
            (w as usize) * 4,
            cs,
            cg::PREMULTIPLIED_FIRST | cg::BYTE_ORDER_32_LITTLE,
        );
        if !ctx.is_null() {
            let r = cg::CGRect { x: 0.0, y: 0.0, w: f64::from(w), h: f64::from(h) };
            cg::CGContextDrawImage(ctx, r, cg);
            cg::CGContextRelease(ctx);
        }
        cg::CGColorSpaceRelease(cs);
    }
    // Our engine takes it as it is: premultiplied, RGBA.
    if let Some(out) = shared.out.lock().ok().and_then(|o| o.clone()) {
        for px in buf.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
        out.picture(ferrocast_av::Picture { width: w, height: h, rgba: buf, premultiplied: true });
    }
}

/// Takes a snapshot when the source shows, then again after one frame's time; stops when
/// the source goes. Main thread.
fn shoot(page: Arc<Mutex<Option<OnMain<Page>>>>, shared: Arc<Shared>) {
    if !shared.alive.load(Ordering::Acquire) {
        return;
    }
    let again = |page: Arc<Mutex<Option<OnMain<Page>>>>, shared: Arc<Shared>, ms: u64| {
        let when = DispatchTime::try_from(Duration::from_millis(ms)).unwrap_or(DispatchTime::NOW);
        let job = OnMain((page, shared));
        let _ = DispatchQueue::main().after(when, move || {
            let OnMain((page, shared)) = job;
            shoot(page, shared);
        });
    };
    let fps = shared.fps.load(Ordering::Relaxed).max(1);
    if !shared.shown.load(Ordering::Acquire) {
        return again(page, shared, 250);
    }
    let view = match page.lock().ok().and_then(|p| p.as_ref().map(|p| p.0.view.clone())) {
        Some(v) => v,
        None => return again(page, shared, 250),
    };
    let started = std::time::Instant::now();
    let (p2, s2) = (page.clone(), shared.clone());
    let done = RcBlock::new(move |img: *mut NSImage, _err: *mut NSError| {
        if !img.is_null() {
            // SAFETY: WebKit hands a live NSImage for the length of this callback.
            copy(unsafe { &*img }, &s2);
        }
        let spent = started.elapsed().as_millis() as u64;
        let frame = 1000 / u64::from(fps);
        again(p2.clone(), s2.clone(), frame.saturating_sub(spent).max(1));
    });
    let mtm = MainThreadMarker::new();
    let Some(mtm) = mtm else { return };
    // SAFETY: snapshot of the live view on the main thread; the block outlives the call
    // (WebKit copies it).
    unsafe {
        let cfg = WKSnapshotConfiguration::new(mtm);
        cfg.setAfterScreenUpdates(false);
        view.takeSnapshotWithConfiguration_completionHandler(Some(&cfg), &done);
    }
}

/// A web page drawn for Ferrocast's own engine (no libobs source): its snapshots and sound
/// go to `feed`. The page closes when this is dropped. Main thread.
pub(crate) struct FedPage {
    shared: Arc<Shared>,
    page: PageSlot,
}

impl std::fmt::Debug for FedPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FedPage").finish_non_exhaustive()
    }
}

impl FedPage {
    /// Opens `url` at `w`×`h` with `css`, snapshotted `fps` times a second into `feed`.
    pub(crate) fn open(
        url: &str,
        w: u32,
        h: u32,
        css: &str,
        fps: u32,
        feed: ferrocast_av::engine::Feed,
    ) -> Option<Self> {
        let mtm = MainThreadMarker::new()?;
        let shared = Arc::new(Shared {
            // Always shown: our engine draws it only where it's on air or in a preview.
            shown: AtomicBool::new(true),
            alive: AtomicBool::new(true),
            w: AtomicU32::new(w.max(1)),
            h: AtomicU32::new(h.max(1)),
            fps: AtomicU32::new(fps.clamp(1, 60)),
            out: Mutex::new(Some(feed)),
        });
        let page: PageSlot = Arc::new(Mutex::new(None));
        let p = open(mtm, &page, &shared, url, w.max(1), h.max(1), css)?;
        if let Ok(mut slot) = page.lock() {
            *slot = Some(OnMain(p));
        }
        shoot(page.clone(), shared.clone());
        Some(Self { shared, page })
    }

    /// Our engine's source changed (the collection was opened again): its new feed.
    pub(crate) fn set_feed(&self, feed: ferrocast_av::engine::Feed) {
        if let Ok(mut o) = self.shared.out.lock() {
            *o = Some(feed);
        }
    }

    /// A new address, size or CSS (the source's settings changed).
    pub(crate) fn update(&self, url: &str, w: u32, h: u32, css: &str, fps: u32) {
        self.shared.w.store(w.max(1), Ordering::Relaxed);
        self.shared.h.store(h.max(1), Ordering::Relaxed);
        self.shared.fps.store(fps.clamp(1, 60), Ordering::Relaxed);
        if let Ok(mut p) = self.page.lock()
            && let Some(OnMain(p)) = p.as_mut()
        {
            if p.size != (w, h) {
                resize(p, w.max(1), h.max(1));
            }
            if p.url != url || p.css != css {
                load(p, url, css);
            }
        }
    }

    /// Clicks, stops clicking or reloads the page.
    pub(crate) fn act(&self, what: Do) {
        let Ok(mut p) = self.page.lock() else { return };
        let Some(OnMain(p)) = p.as_mut() else { return };
        match what {
            Do::Interact(title) => place(p, Some(title)),
            Do::Stop => place(p, None),
            Do::Reload => {
                // SAFETY: reload of the live view on the main thread.
                unsafe {
                    let _ = p.view.reload();
                }
            }
        }
    }
}

impl Drop for FedPage {
    fn drop(&mut self) {
        self.shared.alive.store(false, Ordering::Release);
        if let Ok(mut o) = self.shared.out.lock() {
            *o = None;
        }
        if let Ok(mut p) = self.page.lock()
            && let Some(OnMain(mut p)) = p.take()
        {
            // SAFETY: undoing what `open` registered, on the live objects, main thread.
            unsafe {
                p.view
                    .configuration()
                    .userContentController()
                    .removeScriptMessageHandlerForName(&NSString::from_str(AUDIO_HANDLER));
            }
            p.window.setDelegate(None);
            p.closer = None;
            p.window.close();
        }
    }
}

/// The few CoreGraphics calls the snapshot copy needs.
mod cg {
    use std::ffi::c_void;

    #[repr(C)]
    pub struct CGRect {
        pub x: f64,
        pub y: f64,
        pub w: f64,
        pub h: f64,
    }
    /// kCGImageAlphaPremultipliedFirst and kCGBitmapByteOrder32Little: BGRA in memory.
    pub const PREMULTIPLIED_FIRST: u32 = 2;
    pub const BYTE_ORDER_32_LITTLE: u32 = 2 << 12;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        pub fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
        pub fn CGColorSpaceRelease(cs: *mut c_void);
        pub fn CGBitmapContextCreate(
            data: *mut c_void,
            width: usize,
            height: usize,
            bits_per_component: usize,
            bytes_per_row: usize,
            space: *mut c_void,
            bitmap_info: u32,
        ) -> *mut c_void;
        pub fn CGContextDrawImage(c: *mut c_void, rect: CGRect, image: *mut c_void);
        pub fn CGContextRelease(c: *mut c_void);
    }
}
