//! Screen and window capture with Apple's ScreenCaptureKit (macOS 12.3+): a display or one
//! window, at a size and frame rate, with or without the cursor. Frames arrive on a dispatch
//! queue of ScreenCaptureKit's; the newest is kept (BGRA turned to RGBA) for the compositor.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount, CVPixelBufferGetWidth, CVPixelBufferGetWidthOfPlane,
    CVPixelBufferIsPlanar, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_32ARGB, kCVPixelFormatType_32BGRA, kCVPixelFormatType_32RGBA,
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    kCVPixelFormatType_422YpCbCr8, kCVPixelFormatType_422YpCbCr8_yuvs, kCVPixelFormatType_422YpCbCr8FullRange,
};
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCRunningApplication, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamErrorCode, SCStreamOutput, SCStreamOutputType, SCWindow,
};

use crate::{AvError, Picture};

/// A display that can be captured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Display {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    /// Its top left on the desktop, in points (the main display's is 0,0), as OBS names it.
    pub x: i32,
    pub y: i32,
}

/// A window that can be captured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub id: u32,
    pub title: String,
    pub app: String,
}

/// An app that's running, as a display capture can leave it out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    /// Its bundle id (com.hnc.Discord).
    pub id: String,
    pub name: String,
}

/// How often a display capture that leaves apps out looks for ones started since (an app's
/// first window usually takes longer than this to show).
const PRIVACY_EVERY: Duration = Duration::from_millis(500);

/// Whether `hide` (bundle ids or app names, any case) names this app.
fn hides(hide: &[String], bundle: &str, name: &str) -> bool {
    let name = crate::obs::app_name(name);
    hide.iter().any(|h| {
        let h = crate::obs::app_name(h);
        h.eq_ignore_ascii_case(bundle) || (!name.is_empty() && h.to_lowercase() == name.to_lowercase())
    })
}

/// The running apps `hide` names, and their process ids (to see when that changes).
///
/// # Safety
/// `c` is a live content object.
unsafe fn hidden_apps(c: &SCShareableContent, hide: &[String]) -> (Retained<NSArray<SCRunningApplication>>, Vec<i32>) {
    // SAFETY: reading properties of the live content's apps.
    unsafe {
        let apps: Vec<Retained<SCRunningApplication>> = c
            .applications()
            .iter()
            .filter(|a| hides(hide, &a.bundleIdentifier().to_string(), &a.applicationName().to_string()))
            .collect();
        let mut pids: Vec<i32> = apps.iter().map(|a| a.processID()).collect();
        pids.sort_unstable();
        (NSArray::from_retained_slice(&apps), pids)
    }
}

/// A display's filter, leaving out the apps `hide` names.
///
/// # Safety
/// `c` is a live content object.
unsafe fn display_filter(
    c: &SCShareableContent,
    id: u32,
    hide: &[String],
) -> Result<(Retained<SCContentFilter>, Vec<i32>), AvError> {
    // SAFETY: passed on from the caller's promise.
    unsafe { apps_filter(c, id, hide, false) }
}

/// A display's filter showing only the apps `names` names (OBS's Application Capture), or (not
/// `only`) everything but them.
///
/// # Safety
/// `c` is a live content object.
unsafe fn apps_filter(
    c: &SCShareableContent,
    id: u32,
    names: &[String],
    only: bool,
) -> Result<(Retained<SCContentFilter>, Vec<i32>), AvError> {
    // SAFETY: building a filter from the live content's display and apps.
    unsafe {
        let d: Retained<SCDisplay> =
            c.displays().iter().find(|d| d.displayID() == id).ok_or_else(|| AvError::Source(gone_display(id)))?;
        let none: Retained<NSArray<SCWindow>> = NSArray::new();
        let (apps, pids) = hidden_apps(c, names);
        let filter = if only {
            SCContentFilter::initWithDisplay_includingApplications_exceptingWindows(
                SCContentFilter::alloc(),
                &d,
                &apps,
                &none,
            )
        } else {
            SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
                SCContentFilter::alloc(),
                &d,
                &apps,
                &none,
            )
        };
        Ok((filter, pids))
    }
}

/// How long ScreenCaptureKit gets to answer startup queries.
const WAIT: Duration = Duration::from_secs(10);
/// How long Drop waits for `stopCaptureWithCompletionHandler:` when shutting down a live stream.
const STOP_WAIT: Duration = Duration::from_millis(200);
/// Maximum time `follow_apps` holds frames waiting for `updateContentFilter_completionHandler:`.
const FILTER_WAIT: Duration = Duration::from_secs(2);
/// kCVPixelFormatType_32BGRA.
const BGRA: u32 = kCVPixelFormatType_32BGRA;

fn stop_wait_timeout(was_stopped: bool) -> Option<Duration> {
    (!was_stopped).then_some(STOP_WAIT)
}

fn content() -> Result<Retained<SCShareableContent>, AvError> {
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |c: *mut SCShareableContent, e: *mut NSError| {
        // SAFETY: ScreenCaptureKit passes a live content object or a live error, either may be
        // null; retaining keeps it past the block.
        let r = unsafe { Retained::retain(c) }.ok_or_else(|| {
            // SAFETY: as above, for the error.
            unsafe { Retained::retain(e) }
                .map_or_else(|| "no reason given".to_string(), |e| e.localizedDescription().to_string())
        });
        let _ = tx.send(r);
    });
    // SAFETY: the block lives until ScreenCaptureKit calls it (it's retained by the call).
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    match rx.recv_timeout(WAIT) {
        Ok(Ok(c)) => Ok(c),
        Ok(Err(m)) => Err(AvError::Source(format!(
            "the screen can't be captured ({m}); allow Screen Recording for Ferrocast in System Settings → Privacy & Security"
        ))),
        Err(_) => Err(AvError::Source("macOS didn't answer about screen capture".into())),
    }
}

/// A still of a display or window at most `width` wide (the cards a screen is picked from):
/// the first frame of a small capture that stops at once, so nothing keeps running. macOS 13
/// has no one-shot screenshot call (SCScreenshotManager is 14+).
pub fn still(target: Target, width: u32) -> Result<Picture, AvError> {
    let w = width.clamp(64, 1280);
    // A square box: the picture is scaled to fit it, its own shape kept.
    let cap = ScreenCapture::start(target, w, w, 4, false)?;
    let until = std::time::Instant::now() + Duration::from_millis(1500);
    while std::time::Instant::now() < until {
        if let Some(p) = cap.take() {
            return Ok(p);
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    Err(AvError::Source("the screen gave no picture in time".into()))
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGGetOnlineDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    /// Whether Screen Recording is allowed to this app now. Never shows a prompt.
    fn CGPreflightScreenCaptureAccess() -> bool;
}

/// Whether macOS lets this app record the screen now (System Settings → Privacy & Security),
/// asked without prompting.
pub fn screen_access_granted() -> bool {
    // SAFETY: a plain CoreGraphics query with no arguments.
    unsafe { CGPreflightScreenCaptureAccess() }
}

#[link(name = "ColorSync", kind = "framework")]
unsafe extern "C" {
    fn CGDisplayCreateUUIDFromDisplayID(display: u32) -> super::CFTypeRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFUUIDCreateString(alloc: super::CFAllocatorRef, uuid: super::CFTypeRef) -> super::CFStringRef;
    fn CFStringGetCString(s: super::CFStringRef, buf: *mut std::ffi::c_char, size: isize, encoding: u32) -> u8;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCreateDescriptionFromArray(windows: super::CFTypeRef) -> super::CFTypeRef;
    static kCGWindowOwnerName: super::CFStringRef;
}

/// Who owns window `id` now: `Some(Some(app))` while it's open (minimized, on another Space or
/// in a full-screen one counts: ScreenCaptureKit's list and the window server's "this window"
/// query both leave those out, 19 Nov), `Some(None)` once it's closed, None when the window
/// server can't be asked. A window number can come back for another app's window after a
/// restart, so its app is what says it's the same one.
pub fn window_owner(id: u32) -> Option<Option<String>> {
    // An array of window numbers held as the values themselves (no callbacks), as the window
    // server takes them.
    let ids = [id as usize as *const std::ffi::c_void];
    // SAFETY: one plain value; the array returned is +1 (Owned releases it).
    let ask = super::Owned::new(unsafe { super::CFArrayCreate(std::ptr::null(), ids.as_ptr(), 1, std::ptr::null()) })?;
    // SAFETY: a live array of window numbers; the descriptions returned are +1, or null.
    let list = super::Owned::new(unsafe { CGWindowListCreateDescriptionFromArray(ask.0) })?;
    // SAFETY: a live CFArray of CFDictionaries it owns (not retained here).
    unsafe {
        if super::CFArrayGetCount(list.0) == 0 {
            return Some(None);
        }
        let info = super::CFArrayGetValueAtIndex(list.0, 0);
        let name = super::CFDictionaryGetValue(info, kCGWindowOwnerName.cast());
        if name.is_null() {
            return Some(Some(String::new()));
        }
        let mut buf = [0 as std::ffi::c_char; 512];
        let ok = CFStringGetCString(name, buf.as_mut_ptr(), buf.len() as isize, 0x0800_0100) != 0;
        let text =
            if ok { std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned() } else { String::new() };
        Some(Some(crate::obs::app_name(&text)))
    }
}

/// Whether window `id` is still open (minimized or on another Space counts). Not known: taken
/// as open (never a restart loop on a failed question).
fn window_exists(id: u32) -> bool {
    window_owner(id).is_none_or(|o| o.is_some())
}

/// Why display `id` can't be captured: number 0 is the engine's "its display (by UUID) isn't
/// connected".
fn gone_display(id: u32) -> String {
    if id == 0 {
        "its display isn't connected (plug it in, or pick the display again)".into()
    } else {
        format!("display {id} isn't connected")
    }
}

/// The displays connected now, by number (CoreGraphics', the same as ScreenCaptureKit's):
/// online ones, so a mirrored or sleeping screen still counts as there.
pub(crate) fn active_displays() -> Vec<u32> {
    let mut ids = [0u32; 32];
    let mut n = 0u32;
    // SAFETY: the array and count are ours; at most 32 are written.
    if unsafe { CGGetOnlineDisplayList(32, ids.as_mut_ptr(), &raw mut n) } != 0 {
        return Vec::new();
    }
    ids[..(n as usize).min(32)].to_vec()
}

/// A display's UUID, which stays the same when its number changes (unplugged and plugged
/// back, switched off and on, the Mac restarted with the screens in another order), as OBS
/// keeps it ("display_uuid"). None for a display that isn't connected.
pub fn display_uuid(id: u32) -> Option<String> {
    // SAFETY: a plain lookup; the UUID returned is +1 (Owned releases it).
    let uuid = super::Owned::new(unsafe { CGDisplayCreateUUIDFromDisplayID(id) })?;
    // SAFETY: a live CFUUID; the string returned is +1 (Owned releases it).
    let text = super::Owned::new(unsafe { CFUUIDCreateString(std::ptr::null(), uuid.0) })?;
    let mut buf = [0 as std::ffi::c_char; 64];
    // SAFETY: the buffer and its size are ours; UTF-8 (kCFStringEncodingUTF8).
    if unsafe { CFStringGetCString(text.0, buf.as_mut_ptr(), buf.len() as isize, 0x0800_0100) } == 0 {
        return None;
    }
    // SAFETY: CFStringGetCString wrote a NUL-terminated string into `buf`.
    Some(unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned())
}

/// The display a saved Display Capture means now: the one with its UUID; saved before UUIDs
/// were kept, its number while that's connected, else the only display when there's one.
/// None when it can't be told (its UUID not connected, or several displays and none with its
/// number): never another screen on air by a guess.
pub fn display_now(saved: u32, uuid: Option<&str>) -> Option<u32> {
    let now: Vec<(u32, Option<String>)> = active_displays().into_iter().map(|d| (d, display_uuid(d))).collect();
    pick_display(saved, uuid, &now)
}

/// [`display_now`]'s choice from the displays connected (number, UUID).
fn pick_display(saved: u32, uuid: Option<&str>, now: &[(u32, Option<String>)]) -> Option<u32> {
    if let Some(u) = uuid.filter(|u| !u.is_empty()) {
        return now.iter().find(|d| d.1.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(u))).map(|d| d.0);
    }
    match now {
        _ if now.iter().any(|d| d.0 == saved) => Some(saved),
        [only] => Some(only.0),
        _ => None,
    }
}

/// What can be captured now: displays, and apps' own windows that are on screen and have a title.
pub fn shareable() -> Result<(Vec<Display>, Vec<Window>), AvError> {
    let c = content()?;
    // SAFETY: reading properties of the live content object.
    unsafe {
        let displays = c
            .displays()
            .iter()
            .map(|d| {
                let o = d.frame().origin;
                Display {
                    id: d.displayID(),
                    width: d.width() as u32,
                    height: d.height() as u32,
                    x: o.x.round() as i32,
                    y: o.y.round() as i32,
                }
            })
            .collect();
        // Only apps' own windows on screen, as OBS lists them: the system's layers (the menu bar,
        // the wallpaper, status icons, the Dock) and windows of no app are left out, and so
        // is Ferrocast itself; sorted by app, then title.
        let me = i32::try_from(std::process::id()).unwrap_or(-1);
        let mut windows: Vec<Window> = c
            .windows()
            .iter()
            .filter(|w| w.windowLayer() == 0 && w.isOnScreen())
            .filter_map(|w| {
                let title = w.title()?.to_string();
                let owner = w.owningApplication()?;
                let app = crate::obs::app_name(&owner.applicationName().to_string());
                if title.trim().is_empty() || app.trim().is_empty() || owner.processID() == me {
                    return None;
                }
                Some(Window { id: w.windowID(), title, app })
            })
            .collect();
        windows.sort_by(|a, b| {
            (a.app.to_lowercase(), a.title.to_lowercase()).cmp(&(b.app.to_lowercase(), b.title.to_lowercase()))
        });
        Ok((displays, windows))
    }
}

/// The running apps (with a bundle id), by name: what a display capture can leave out.
pub fn apps() -> Result<Vec<App>, AvError> {
    let c = content()?;
    // SAFETY: reading properties of the live content's apps.
    let mut apps: Vec<App> = unsafe {
        c.applications()
            .iter()
            .map(|a| App {
                id: a.bundleIdentifier().to_string(),
                name: crate::obs::app_name(&a.applicationName().to_string()),
            })
            .filter(|a| !a.id.is_empty())
            .collect()
    };
    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));
    apps.dedup_by(|a, b| a.id == b.id);
    Ok(apps)
}

/// The displays an app (by bundle id, any case) has windows showing on now, by the middle of
/// each window: an app capture of another display shows nothing, and this says where to look.
pub fn app_displays(bundle: &str) -> Result<Vec<u32>, AvError> {
    let c = content()?;
    // SAFETY: reading properties of the live content's displays and windows.
    unsafe {
        let displays: Vec<(u32, objc2_core_foundation::CGRect)> =
            c.displays().iter().map(|d| (d.displayID(), d.frame())).collect();
        let mut out: Vec<u32> = c
            .windows()
            .iter()
            .filter(|w| w.windowLayer() == 0 && w.isOnScreen())
            .filter(|w| {
                w.owningApplication().is_some_and(|a| a.bundleIdentifier().to_string().eq_ignore_ascii_case(bundle))
            })
            .filter_map(|w| {
                let f = w.frame();
                let (x, y) = (f.origin.x + f.size.width / 2.0, f.origin.y + f.size.height / 2.0);
                displays
                    .iter()
                    .find(|(_, d)| {
                        x >= d.origin.x
                            && x < d.origin.x + d.size.width
                            && y >= d.origin.y
                            && y < d.origin.y + d.size.height
                    })
                    .map(|(id, _)| *id)
            })
            .collect();
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }
}

/// What the stream's output keeps: the newest frame and how many came, and the sound
/// (interleaved stereo float at 48 kHz, each piece with its host time) when it's captured.
#[derive(Debug, Default)]
struct Shared {
    latest: Mutex<Option<Surface>>,
    frames: AtomicU64,
    sound: Mutex<std::collections::VecDeque<(Vec<f32>, u64)>>,
    /// The picture holds (no new frame kept) while an app to leave out isn't left out yet.
    hold: AtomicBool,
    /// A window capture: its stream, kept at the window's own size as it is resized.
    fit: Mutex<Option<WindowFit>>,
    /// Set as soon as ScreenCaptureKit stops the stream (or Drop begins) so Drop and privacy stop cleanly.
    has_stopped: AtomicBool,
    /// Why ScreenCaptureKit stopped the stream (`SCStreamDelegate::stream:didStopWithError:`).
    stopped: Mutex<Option<String>>,
}

/// How a capture's end for a turned-off permission begins (the watchdog tells it from a crash).
pub(crate) const PERMISSION_REVOKED: &str = "screen recording permission was revoked";

fn format_stream_error(code: isize, desc: &str) -> String {
    match SCStreamErrorCode(code) {
        SCStreamErrorCode::UserDeclined | SCStreamErrorCode::MissingEntitlements => {
            format!("{PERMISSION_REVOKED} ({desc})")
        }
        SCStreamErrorCode::NoCaptureSource | SCStreamErrorCode::NoDisplayList | SCStreamErrorCode::NoWindowList => {
            format!("the captured display or window is gone ({desc})")
        }
        SCStreamErrorCode::UserStopped | SCStreamErrorCode::SystemStoppedStream => {
            format!("macOS stopped screen capture ({desc})")
        }
        SCStreamErrorCode::FailedApplicationConnectionInvalid
        | SCStreamErrorCode::FailedApplicationConnectionInterrupted => {
            format!("macOS screen capture connection was interrupted ({desc})")
        }
        _ => format!("screen capture stopped ({desc})"),
    }
}

fn record_stream_error(shared: &Shared, reason: String) {
    shared.has_stopped.store(true, Ordering::Release);
    shared.hold.store(false, Ordering::Release);
    if let Ok(mut stopped) = shared.stopped.lock()
        && stopped.is_none()
    {
        *stopped = Some(reason);
    }
    // Break SCStream -> Output -> Shared -> WindowFit -> SCStream retain cycle on stream failure.
    if let Ok(mut fit) = shared.fit.lock() {
        *fit = None;
    }
}

/// What a window capture needs to follow the window's size, as OBS's ScreenCaptureKit capture
/// does: each frame says how big the window is now, and a new size is asked for when it
/// changed. Asked for once, at the start, a resized window was drawn small inside the old
/// size with black round it, and its box on the canvas no longer matched it.
struct WindowFit {
    stream: SendStream,
    config: SendConfig,
    /// The most a side may be (the canvas), and the size asked for now.
    max: (u32, u32),
    size: (u32, u32),
}

impl std::fmt::Debug for WindowFit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowFit").field("max", &self.max).field("size", &self.size).finish_non_exhaustive()
    }
}

/// A stream's settings, changed from ScreenCaptureKit's queue.
struct SendConfig(Retained<SCStreamConfiguration>);
// SAFETY: a configuration object, only changed on the one serial queue its stream's frames
// come on, then handed to the stream (which copies it).
unsafe impl Send for SendConfig {}

impl Shared {
    /// After a window frame: the window's size now, and a new stream size when it changed.
    fn follow_window(&self, sample: &CMSampleBuffer) {
        let Ok(mut fit) = self.fit.lock() else { return };
        let Some(fit) = fit.as_mut() else { return };
        let Some(px) = window_pixels(sample) else { return };
        let want = window_size(px, fit.max);
        // Any change, a pixel too: a size even a pixel off is scaled, every pixel blurred.
        if want == fit.size {
            return;
        }
        fit.size = want;
        // SAFETY: the stream and its configuration are live; the new size is in range.
        unsafe {
            fit.config.0.setWidth(want.0.clamp(16, 8192) as usize);
            fit.config.0.setHeight(want.1.clamp(16, 8192) as usize);
            fit.stream.0.updateConfiguration_completionHandler(&fit.config.0, None);
        }
    }
}

/// A window frame's window size in pixels: its content rectangle (points), times points to
/// pixels, over how much it was scaled to fit the frame (ScreenCaptureKit's frame info).
fn window_pixels(sample: &CMSampleBuffer) -> Option<(f64, f64)> {
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSDictionary, NSNumber, NSString};
    use objc2_screen_capture_kit::{
        SCStreamFrameInfoContentRect, SCStreamFrameInfoContentScale, SCStreamFrameInfoScaleFactor,
    };
    // SAFETY: the sample buffer is live for this call; its attachments array is a CFArray of
    // CFDictionary, toll-free bridged to NSArray / NSDictionary.
    unsafe {
        let arr = sample.sample_attachments_array(false)?;
        let arr: &NSArray<NSDictionary<NSString, AnyObject>> = &*(CFRetained::as_ptr(&arr).as_ptr().cast());
        let info = arr.firstObject()?;
        let number = |k: &NSString| info.objectForKey(k)?.downcast::<NSNumber>().ok().map(|n| n.doubleValue());
        let rect = info.objectForKey(SCStreamFrameInfoContentRect)?.downcast::<NSDictionary>().ok()?;
        let side =
            |k: &str| rect.objectForKey(&NSString::from_str(k))?.downcast::<NSNumber>().ok().map(|n| n.doubleValue());
        let (w, h) = (side("Width")?, side("Height")?);
        let scale = number(SCStreamFrameInfoScaleFactor).filter(|s| *s > 0.0).unwrap_or(1.0);
        let shrunk = number(SCStreamFrameInfoContentScale).filter(|s| *s > 0.0).unwrap_or(1.0);
        Some((w * scale / shrunk, h * scale / shrunk))
    }
}

/// Sound pieces kept before they're taken (about 5 s).
const MAX_SOUND: usize = 500;

define_class!(
    // SAFETY: NSObject may be subclassed; the ivars are an Arc (dropped with the object).
    #[unsafe(super(NSObject))]
    #[name = "FerrocastScreenOutput"]
    #[ivars = Arc<Shared>]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if self.ivars().has_stopped.load(Ordering::Acquire) {
                return;
            }
            if kind == SCStreamOutputType::Audio {
                if let Some(piece) = sound_of(sample)
                    && let Ok(mut q) = self.ivars().sound.lock()
                {
                    q.push_back(piece);
                    while q.len() > MAX_SOUND {
                        q.pop_front();
                    }
                }
                return;
            }
            if kind != SCStreamOutputType::Screen {
                return;
            }
            if self.ivars().hold.load(Ordering::Acquire) {
                return;
            }
            self.ivars().follow_window(sample);
            // SAFETY: the sample buffer is live for this call; an idle frame has no image.
            let Some(image) = (unsafe { sample.image_buffer() }) else { return };
            // Kept as the system holds it, not copied here: the engine copies it once, straight to
            // the card, when it draws (a copy and a colour swap of every frame on this queue was
            // the biggest CPU cost of a 1080p recording).
            // One that can't be taken (not BGRA, no size) leaves the last good one, uncounted.
            if let Some(surface) = Surface::of(image)
                && let Ok(mut l) = self.ivars().latest.lock()
            {
                *l = Some(surface);
                self.ivars().frames.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: Option<&NSError>) {
            let reason = match error {
                Some(e) => format_stream_error(e.code(), &e.localizedDescription().to_string()),
                None => "screen capture stopped unexpectedly".into(),
            };
            record_stream_error(self.ivars(), reason);
        }
    }
);

impl Output {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this: Allocated<Self> = Self::alloc();
        let this = this.set_ivars(shared);
        // SAFETY: NSObject's init on a freshly allocated object with its ivars set.
        unsafe { msg_send![super(this), init] }
    }
}

/// ScreenCaptureKit's sound: 32-bit float, one plane per channel (left, then right), in one
/// block → interleaved stereo, with its host time.
fn sound_of(sample: &CMSampleBuffer) -> Option<(Vec<f32>, u64)> {
    // SAFETY: a live sample buffer; its block holds `frames` floats per channel, planes one
    // after the other (ScreenCaptureKit's layout for the channel count asked).
    unsafe {
        let frames = usize::try_from(sample.num_samples()).ok()?;
        let block = sample.data_buffer()?;
        let len = block.data_length();
        if frames == 0 || len < frames * 4 {
            return None;
        }
        let mut bytes = vec![0u8; len];
        let dst = std::ptr::NonNull::new(bytes.as_mut_ptr().cast())?;
        if block.copy_data_bytes(0, len, dst) != 0 {
            return None;
        }
        let all: Vec<f32> = bytes.as_chunks::<4>().0.iter().map(|b| f32::from_ne_bytes(*b)).collect();
        let (l, r) = if all.len() >= frames * 2 {
            (&all[..frames], &all[frames..frames * 2])
        } else {
            (&all[..frames], &all[..frames])
        };
        let t = sample.presentation_time_stamp();
        let ts = if t.timescale > 0 && t.value >= 0 {
            (i128::from(t.value) * 1_000_000_000 / i128::from(t.timescale)) as u64
        } else {
            0
        };
        Some((l.iter().zip(r).flat_map(|(a, b)| [*a, *b]).collect(), ts))
    }
}

/// Reads the screen at its full pixels (macOS 14's captureResolution "best"): left to
/// "automatic", ScreenCaptureKit may read a Retina screen at its point size and scale that
/// up, a softer picture than the screen's own. macOS 13 has no setting (it reads the pixels).
fn best_resolution(config: &SCStreamConfiguration) {
    // SCCaptureResolutionBest.
    const BEST: isize = 1;
    let sel = objc2::sel!(setCaptureResolution:);
    // SAFETY: NSObject's respondsToSelector: on a live configuration; then, where it's there,
    // its setCaptureResolution: with an SCCaptureResolutionType (NSInteger) value.
    unsafe {
        let has: bool = objc2::msg_send![config, respondsToSelector: sel];
        if has {
            let _: () = objc2::msg_send![config, setCaptureResolution: BEST];
        }
    }
}

/// A window's capture size: its own pixels exactly, made smaller (never larger) only to fit in
/// `max` with its shape kept. Exactly: ScreenCaptureKit scales the window to the size asked
/// for, and a size a pixel off (rounded to even, 1917 asked as 1918) resampled every pixel
/// and blurred its text (owner's OBS comparison, 12 Nov). Nothing needs it even: the window
/// is drawn on the canvas, and a recording of it alone crops to even sizes instead.
fn window_size((w, h): (f64, f64), max: (u32, u32)) -> (u32, u32) {
    if !(w >= 1.0 && h >= 1.0) {
        return max;
    }
    let f = (f64::from(max.0) / w).min(f64::from(max.1) / h).min(1.0);
    let side = |v: f64| ((v * f).round() as u32).max(16);
    (side(w), side(h))
}

/// A frame of a display as the system holds it: BGRA, colours multiplied by alpha, not copied.
/// It is kept (so ScreenCaptureKit can't reuse its memory) until it has been sent to the card.
/// Cloning keeps the same buffer one more time (no pixels are copied).
#[derive(Clone)]
pub struct Surface {
    image: CFRetained<objc2_core_video::CVImageBuffer>,
    width: u32,
    height: u32,
    #[cfg(test)]
    copy_counter: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
}

// SAFETY: a CoreVideo buffer is reference counted and the system itself passes it between its
// queues; it is only read, under CoreVideo's own lock, and let go from any thread.
unsafe impl Send for Surface {}

impl std::fmt::Debug for Surface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Surface({}x{})", self.width, self.height)
    }
}

impl Surface {
    /// None for a buffer that isn't non-planar 32-bit BGRA or has no size.
    pub(crate) fn of(image: CFRetained<objc2_core_video::CVImageBuffer>) -> Option<Self> {
        let (w, h) = (CVPixelBufferGetWidth(&image), CVPixelBufferGetHeight(&image));
        if w == 0 || h == 0 || CVPixelBufferIsPlanar(&image) || CVPixelBufferGetPixelFormatType(&image) != BGRA {
            return None;
        }
        Some(Self {
            image,
            width: u32::try_from(w).ok()?,
            height: u32::try_from(h).ok()?,
            #[cfg(test)]
            copy_counter: None,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Its pixels copied out as a picture of ours (RGBA), for whoever needs them on the CPU.
    pub fn copy(&self) -> Option<Picture> {
        copy_bgra(&self.image)
    }

    /// The IOSurface the pixels live in, when the buffer has one (ScreenCaptureKit's always do).
    pub fn io_surface(&self) -> Option<CFRetained<objc2_io_surface::IOSurfaceRef>> {
        // Retained for us by the binding (the "Get" rule's retain is done there).
        objc2_core_video::CVPixelBufferGetIOSurface(Some(&self.image))
    }

    /// Reads the pixels where they are: `f` gets them (BGRA, `stride` bytes a row), or None when
    /// the system won't let them be read.
    pub fn with_bytes<R>(&self, f: impl FnOnce(&[u8], usize) -> R) -> Option<R> {
        if CVPixelBufferIsPlanar(&self.image) || CVPixelBufferGetPixelFormatType(&self.image) != BGRA {
            return None;
        }
        let _lock = PixelLock::read_only(&self.image)?;
        let stride = CVPixelBufferGetBytesPerRow(&self.image);
        let base = CVPixelBufferGetBaseAddress(&self.image).cast::<u8>();
        let h = self.height as usize;
        if base.is_null() || stride < self.width as usize * 4 {
            return None;
        }
        // SAFETY: the locked buffer holds `h` rows of `stride` bytes at `base`, and nothing
        // writes to it while it is locked for reading.
        Some(f(unsafe { std::slice::from_raw_parts(base, stride * h) }, stride))
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn with_copy_counter(mut self, counter: std::sync::Arc<std::sync::atomic::AtomicU64>) -> Self {
        self.copy_counter = Some(counter);
        self
    }

    /// The frame as an RGBA picture (a copy, for the ways that need one).
    pub fn to_picture(&self) -> Option<Picture> {
        #[cfg(test)]
        if let Some(c) = &self.copy_counter {
            c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        copy_bgra(&self.image)
    }
}

struct PixelLock<'a>(&'a objc2_core_video::CVPixelBuffer);

impl<'a> PixelLock<'a> {
    fn read_only(image: &'a objc2_core_video::CVPixelBuffer) -> Option<Self> {
        // SAFETY: locking a live pixel buffer read-only; unlocked on Drop with the same flag.
        (unsafe { CVPixelBufferLockBaseAddress(image, CVPixelBufferLockFlags::ReadOnly) } == 0).then_some(Self(image))
    }
}

impl Drop for PixelLock<'_> {
    fn drop(&mut self) {
        // SAFETY: locked read-only in `read_only`.
        unsafe { CVPixelBufferUnlockBaseAddress(self.0, CVPixelBufferLockFlags::ReadOnly) };
    }
}

/// A BGRA pixel buffer's pixels as RGBA rows (premultiplied, as ScreenCaptureKit gives them).
pub(crate) fn copy_bgra(image: &objc2_core_video::CVPixelBuffer) -> Option<Picture> {
    if CVPixelBufferIsPlanar(image) || CVPixelBufferGetPixelFormatType(image) != BGRA {
        return None;
    }
    let _lock = PixelLock::read_only(image)?;
    let (w, h, stride) =
        (CVPixelBufferGetWidth(image), CVPixelBufferGetHeight(image), CVPixelBufferGetBytesPerRow(image));
    let (w_u32, h_u32) = (u32::try_from(w).ok()?, u32::try_from(h).ok()?);
    let base = CVPixelBufferGetBaseAddress(image).cast::<u8>();
    if base.is_null() || w == 0 || h == 0 || stride < w * 4 {
        return None;
    }
    // Each pixel is turned as one 32-bit word (blue and red swapped) so the loop compiles to
    // wide vector moves; a byte-by-byte `extend_from_slice` per pixel was the biggest CPU
    // cost in a 1080p recording.
    // A kept buffer when there is one (every byte is written below): a fresh one each frame was
    // page faults and zero-filling on the thread that decodes the clip.
    let mut rgba = crate::pixel_buffer(w * h * 4);
    for (y, out) in rgba.chunks_exact_mut(w * 4).enumerate() {
        // SAFETY: the locked buffer holds `h` rows of `stride` bytes at `base`.
        let row = unsafe { std::slice::from_raw_parts(base.add(y * stride), w * 4) };
        for (d, s) in out.as_chunks_mut::<4>().0.iter_mut().zip(row.as_chunks::<4>().0) {
            let v = u32::from_le_bytes(*s);
            *d = ((v & 0xFF00_FF00) | (v << 16 & 0x00FF_0000) | (v >> 16 & 0x0000_00FF)).to_le_bytes();
        }
    }
    Some(Picture { width: w_u32, height: h_u32, rgba, premultiplied: true })
}

/// Converts any supported CoreVideo camera/capture buffer (`32BGRA`, `32ARGB`, `32RGBA`,
/// bi-planar `420v`/`420f`, or packed 4:2:2 `2vuy`/`yuvs`/`yuvf`) safely to RGBA. Returns
/// `None` instead of misreading stride/planes for unknown formats.
pub(crate) fn copy_video_frame(image: &objc2_core_video::CVPixelBuffer, bt709: bool) -> Option<Picture> {
    let fmt = CVPixelBufferGetPixelFormatType(image);
    if fmt == kCVPixelFormatType_32BGRA {
        return copy_bgra(image);
    }
    if fmt == kCVPixelFormatType_32ARGB || fmt == kCVPixelFormatType_32RGBA {
        return copy_packed_rgba32(image, fmt == kCVPixelFormatType_32ARGB);
    }
    if fmt == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange || fmt == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange {
        let full = fmt == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;
        return copy_biplanar_420(image, bt709, full);
    }
    if fmt == kCVPixelFormatType_422YpCbCr8
        || fmt == kCVPixelFormatType_422YpCbCr8_yuvs
        || fmt == kCVPixelFormatType_422YpCbCr8FullRange
    {
        let full = fmt == kCVPixelFormatType_422YpCbCr8FullRange;
        let u_first = fmt == kCVPixelFormatType_422YpCbCr8;
        return copy_packed_422(image, bt709, full, u_first);
    }
    None
}

fn copy_packed_rgba32(image: &objc2_core_video::CVPixelBuffer, argb: bool) -> Option<Picture> {
    if CVPixelBufferIsPlanar(image) {
        return None;
    }
    let _lock = PixelLock::read_only(image)?;
    let (w, h, stride) =
        (CVPixelBufferGetWidth(image), CVPixelBufferGetHeight(image), CVPixelBufferGetBytesPerRow(image));
    let (w_u32, h_u32) = (u32::try_from(w).ok()?, u32::try_from(h).ok()?);
    let base = CVPixelBufferGetBaseAddress(image).cast::<u8>();
    if base.is_null() || w == 0 || h == 0 || stride < w * 4 {
        return None;
    }
    let mut rgba = vec![0u8; w * h * 4];
    for (y, out) in rgba.chunks_exact_mut(w * 4).enumerate() {
        // SAFETY: the locked buffer holds `h` rows of `stride` bytes at `base`.
        let row = unsafe { std::slice::from_raw_parts(base.add(y * stride), w * 4) };
        if argb {
            for (d, s) in out.as_chunks_mut::<4>().0.iter_mut().zip(row.as_chunks::<4>().0) {
                *d = [s[1], s[2], s[3], s[0]];
            }
        } else {
            out.copy_from_slice(row);
        }
    }
    Some(Picture { width: w_u32, height: h_u32, rgba, premultiplied: true })
}

#[derive(Clone, Copy)]
struct YuvCoeffs {
    ry: f32,
    ru: f32,
    rv: f32,
    rc: f32,
    gy: f32,
    gu: f32,
    gv: f32,
    gc: f32,
    by: f32,
    bu: f32,
    bv: f32,
    bc: f32,
}

impl YuvCoeffs {
    fn new(bt709: bool, full: bool) -> Self {
        let r = crate::compose::yuv_rows(bt709, full);
        Self {
            ry: r[0],
            ru: r[1],
            rv: r[2],
            rc: r[3] * 255.0 + 0.5,
            gy: r[4],
            gu: r[5],
            gv: r[6],
            gc: r[7] * 255.0 + 0.5,
            by: r[8],
            bu: r[9],
            bv: r[10],
            bc: r[11] * 255.0 + 0.5,
        }
    }

    #[inline]
    fn rgba(self, y: u8, u: u8, v: u8) -> [u8; 4] {
        let (yf, uf, vf) = (f32::from(y), f32::from(u), f32::from(v));
        let clamp = |c: f32| c.clamp(0.0, 255.0) as u8;
        [
            clamp(self.ry * yf + self.ru * uf + self.rv * vf + self.rc),
            clamp(self.gy * yf + self.gu * uf + self.gv * vf + self.gc),
            clamp(self.by * yf + self.bu * uf + self.bv * vf + self.bc),
            255,
        ]
    }
}

fn copy_biplanar_420(image: &objc2_core_video::CVPixelBuffer, bt709: bool, full: bool) -> Option<Picture> {
    if !CVPixelBufferIsPlanar(image) || CVPixelBufferGetPlaneCount(image) < 2 {
        return None;
    }
    let _lock = PixelLock::read_only(image)?;
    let (w, h) = (CVPixelBufferGetWidth(image), CVPixelBufferGetHeight(image));
    if w == 0 || h == 0 {
        return None;
    }
    let (w_u32, h_u32) = (u32::try_from(w).ok()?, u32::try_from(h).ok()?);
    let y_w = CVPixelBufferGetWidthOfPlane(image, 0);
    let y_h = CVPixelBufferGetHeightOfPlane(image, 0);
    let uv_w = CVPixelBufferGetWidthOfPlane(image, 1);
    let uv_h = CVPixelBufferGetHeightOfPlane(image, 1);
    let y_stride = CVPixelBufferGetBytesPerRowOfPlane(image, 0);
    let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(image, 1);
    let y_base = CVPixelBufferGetBaseAddressOfPlane(image, 0).cast::<u8>();
    let uv_base = CVPixelBufferGetBaseAddressOfPlane(image, 1).cast::<u8>();
    let min_uv_bytes = w.div_ceil(2) * 2;
    if y_base.is_null()
        || uv_base.is_null()
        || y_w < w
        || y_h < h
        || uv_w < w.div_ceil(2)
        || uv_h < h.div_ceil(2)
        || y_stride < w
        || uv_stride < min_uv_bytes
    {
        return None;
    }
    let coeffs = YuvCoeffs::new(bt709, full);
    let mut rgba = vec![0u8; w * h * 4];
    for (y, out) in rgba.chunks_exact_mut(w * 4).enumerate() {
        // SAFETY: plane 0 holds at least `h` rows of `y_stride >= w` bytes; plane 1 holds `h.div_ceil(2)` rows of `uv_stride >= min_uv_bytes` bytes.
        let (y_row, uv_row) = unsafe {
            (
                std::slice::from_raw_parts(y_base.add(y * y_stride), w),
                std::slice::from_raw_parts(uv_base.add((y / 2) * uv_stride), min_uv_bytes),
            )
        };
        for (x, px) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let uv = (x / 2) * 2;
            *px = coeffs.rgba(y_row[x], uv_row[uv], uv_row[uv + 1]);
        }
    }
    Some(Picture { width: w_u32, height: h_u32, rgba, premultiplied: true })
}

fn copy_packed_422(image: &objc2_core_video::CVPixelBuffer, bt709: bool, full: bool, u_first: bool) -> Option<Picture> {
    if CVPixelBufferIsPlanar(image) {
        return None;
    }
    let _lock = PixelLock::read_only(image)?;
    let (w, h, stride) =
        (CVPixelBufferGetWidth(image), CVPixelBufferGetHeight(image), CVPixelBufferGetBytesPerRow(image));
    let (w_u32, h_u32) = (u32::try_from(w).ok()?, u32::try_from(h).ok()?);
    let base = CVPixelBufferGetBaseAddress(image).cast::<u8>();
    let min_row_bytes = w.div_ceil(2) * 4;
    if base.is_null() || w == 0 || h == 0 || stride < min_row_bytes {
        return None;
    }
    let coeffs = YuvCoeffs::new(bt709, full);
    let mut rgba = vec![0u8; w * h * 4];
    for (y, out) in rgba.chunks_exact_mut(w * 4).enumerate() {
        // SAFETY: the locked buffer holds `h` rows of `stride >= min_row_bytes` bytes.
        let row = unsafe { std::slice::from_raw_parts(base.add(y * stride), min_row_bytes) };
        let dst = out.as_chunks_mut::<4>().0;
        for (pair_idx, m) in row.as_chunks::<4>().0.iter().enumerate() {
            let (y0, u, y1, v) = if u_first { (m[1], m[0], m[3], m[2]) } else { (m[0], m[1], m[2], m[3]) };
            let x0 = pair_idx * 2;
            dst[x0] = coeffs.rgba(y0, u, v);
            if x0 + 1 < w {
                dst[x0 + 1] = coeffs.rgba(y1, u, v);
            }
        }
    }
    Some(Picture { width: w_u32, height: h_u32, rgba, premultiplied: true })
}

struct PrivacyStop {
    flag: Arc<AtomicBool>,
    wake: mpsc::Sender<()>,
}

impl PrivacyStop {
    fn signal(&self) {
        self.flag.store(true, Ordering::Release);
        let _ = self.wake.send(());
    }
}

/// A running capture; dropping it stops.
pub struct ScreenCapture {
    stream: Retained<SCStream>,
    output: Retained<Output>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
    shared: Arc<Shared>,
    /// Tells the thread that keeps apps out (when there is one) to stop.
    privacy: Option<PrivacyStop>,
    /// The window captured (a Window Capture): its closing ends the capture.
    window: Option<u32>,
    /// The display captured (a Display or Application Capture): its going (unplugged)
    /// ends the capture, so the watchdog opens it again, found by its UUID, when it's back.
    display: Option<u32>,
}

/// The stream, handed to the thread that keeps apps out.
struct SendStream(Retained<SCStream>);
// SAFETY: ScreenCaptureKit's stream may have its filter changed from any thread.
unsafe impl Send for SendStream {}

impl std::fmt::Debug for ScreenCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenCapture").field("frames", &self.frames()).finish()
    }
}

// SAFETY: ScreenCaptureKit's stream may be started and stopped from any thread; the shared
// state is behind a mutex and atomics.
unsafe impl Send for ScreenCapture {}

/// What to capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Display(u32),
    Window(u32),
}

impl ScreenCapture {
    /// Starts capturing at `width`×`height` (scaled to fit), up to `fps` frames a second.
    pub fn start(target: Target, width: u32, height: u32, fps: u32, cursor: bool) -> Result<Self, AvError> {
        Self::start_with(target, width, height, fps, cursor, false, &[])
    }

    /// The same, and with `sound`, what the Mac plays too (all apps, or the window's app),
    /// leaving out Ferrocast's own: OBS's macOS desktop audio, with no driver to install.
    /// A display leaves out the windows of the apps `hide` names (bundle ids or names): the
    /// privacy guard, so a message or a notification never reaches the stream. Apps opened later
    /// are left out too, within a couple of seconds' look.
    pub fn start_with(
        target: Target,
        width: u32,
        height: u32,
        fps: u32,
        cursor: bool,
        sound: bool,
        hide: &[String],
    ) -> Result<Self, AvError> {
        if let Target::Window(0) = target {
            return Err(AvError::Source("its window is closed (open it, or pick the window again)".into()));
        }
        Self::open(target, None, width, height, fps, cursor, sound, hide)
    }

    /// Only the windows of the app `app` names (its bundle id or name) on `display`, wherever
    /// they move on it (OBS's Application Capture); with `sound`, that app's sound. Followed
    /// when the app quits and starts again.
    pub fn start_app(
        display: u32,
        app: &str,
        width: u32,
        height: u32,
        fps: u32,
        cursor: bool,
        sound: bool,
    ) -> Result<Self, AvError> {
        if app.trim().is_empty() {
            return Err(AvError::Source("no app picked to capture".into()));
        }
        Self::open(Target::Display(display), Some(app.trim()), width, height, fps, cursor, sound, &[])
    }

    #[allow(clippy::too_many_arguments)]
    fn open(
        target: Target,
        only: Option<&str>,
        width: u32,
        height: u32,
        fps: u32,
        cursor: bool,
        sound: bool,
        hide: &[String],
    ) -> Result<Self, AvError> {
        let only: Option<Vec<String>> = only.map(|a| vec![a.to_string()]);
        let c = content()?;
        // SAFETY: building ScreenCaptureKit objects from the live content's displays and
        // windows, on this thread; each call gets live arguments.
        unsafe {
            let mut hidden = Vec::new();
            let filter = match target {
                Target::Display(id) if let Some(only) = &only => {
                    let (f, pids) = apps_filter(&c, id, only, true)?;
                    hidden = pids;
                    f
                }
                Target::Display(id) => {
                    let (f, pids) = display_filter(&c, id, hide)?;
                    hidden = pids;
                    f
                }
                Target::Window(id) => {
                    let w: Retained<SCWindow> = c.windows().iter().find(|w| w.windowID() == id).ok_or_else(|| {
                        AvError::Source(if id == 0 {
                            "its window is closed (open it, or pick the window again)".into()
                        } else {
                            format!("window {id} is gone")
                        })
                    })?;
                    SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &w)
                }
            };
            let (max_w, max_h) = (width, height);
            // A window is captured at its own shape: asked for the canvas's 16:9, ScreenCaptureKit
            // drew a window of another shape into a corner of a black picture, and the source's
            // box fitted that black picture, the window small inside it.
            let (width, height) = match target {
                Target::Window(_) => {
                    let r = filter.contentRect();
                    let s = f64::from(filter.pointPixelScale()).max(1.0);
                    window_size((r.size.width * s, r.size.height * s), (width, height))
                }
                Target::Display(_) => (width, height),
            };
            let config = SCStreamConfiguration::new();
            config.setWidth(width.clamp(16, 8192) as usize);
            config.setHeight(height.clamp(16, 8192) as usize);
            config.setScalesToFit(true);
            config.setPixelFormat(BGRA);
            best_resolution(&config);
            config.setMinimumFrameInterval(CMTime {
                value: 1,
                timescale: fps.clamp(1, 240) as i32,
                flags: objc2_core_media::CMTimeFlags::Valid,
                epoch: 0,
            });
            config.setShowsCursor(cursor);
            config.setQueueDepth(5);
            if sound {
                config.setCapturesAudio(true);
                config.setSampleRate(48_000);
                config.setChannelCount(2);
                config.setExcludesCurrentProcessAudio(true);
            }
            let shared = Arc::new(Shared::default());
            let output = Output::new(shared.clone());
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*output)),
            );
            if let Target::Window(_) = target
                && let Ok(mut fit) = shared.fit.lock()
            {
                *fit = Some(WindowFit {
                    stream: SendStream(stream.clone()),
                    config: SendConfig(config.clone()),
                    max: (max_w, max_h),
                    size: (width, height),
                });
            }
            let clear_fit = || {
                if let Ok(mut fit) = shared.fit.lock() {
                    *fit = None;
                }
            };
            let queue = DispatchQueue::new("ferrocast.screen", DispatchQueueAttr::SERIAL);
            if let Err(e) = stream.addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Screen,
                Some(&queue),
            ) {
                clear_fit();
                return Err(AvError::Source(format!("the capture couldn't start ({})", e.localizedDescription())));
            }
            if sound
                && let Err(e) = stream.addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Audio,
                    Some(&queue),
                )
            {
                clear_fit();
                return Err(AvError::Source(format!(
                    "the Mac's sound couldn't be captured ({})",
                    e.localizedDescription()
                )));
            }
            let (tx, rx) = mpsc::channel();
            let done = RcBlock::new(move |e: *mut NSError| {
                // SAFETY: the error, when given, is live for the block's call.
                let r = match Retained::retain(e) {
                    None => Ok(()),
                    Some(e) => Err(e.localizedDescription().to_string()),
                };
                let _ = tx.send(r);
            });
            stream.startCaptureWithCompletionHandler(Some(&done));
            match rx.recv_timeout(WAIT) {
                Ok(Ok(())) => {}
                Ok(Err(m)) => {
                    clear_fit();
                    return Err(AvError::Source(format!("the capture couldn't start ({m})")));
                }
                Err(_) => {
                    clear_fit();
                    return Err(AvError::Source("macOS didn't start the capture".into()));
                }
            }
            let privacy = match (target, only) {
                (Target::Display(id), Some(only)) => {
                    Some(follow_apps(SendStream(stream.clone()), shared.clone(), id, only, true, hidden))
                }
                (Target::Display(id), None) if !hide.is_empty() => {
                    Some(follow_apps(SendStream(stream.clone()), shared.clone(), id, hide.to_vec(), false, hidden))
                }
                _ => None,
            };
            let (window, display) = match target {
                Target::Window(id) => (Some(id), None),
                Target::Display(id) => (None, Some(id)),
            };
            Ok(Self { stream, output, _queue: queue, shared, privacy, window, display })
        }
    }

    /// The newest frame as an RGBA picture (taken: the next call gives the one after, or None
    /// until it comes). A copy: the engine uses `take_surface`.
    pub fn take(&self) -> Option<Picture> {
        self.take_surface().and_then(|s| s.to_picture())
    }

    /// The newest frame as the system holds it, for sending to the card without a copy of ours.
    pub fn take_surface(&self) -> Option<Surface> {
        self.shared.latest.lock().ok().and_then(|mut l| l.take())
    }

    /// Sound since the last call (when started with sound), oldest first, with host times.
    pub fn take_sound(&self) -> Vec<(Vec<f32>, u64)> {
        self.shared.sound.lock().map(|mut s| s.drain(..).collect()).unwrap_or_default()
    }

    /// ScreenCaptureKit sets `excludesCurrentProcessAudio = true`, so captured sound excludes
    /// Ferrocast's own monitor playback.
    pub fn excludes_self_audio(&self) -> bool {
        true
    }

    /// Output device id for non-self-excluding loopback (`None` here: SCK excludes self).
    pub fn loopback_device(&self) -> Option<&str> {
        None
    }

    /// Frames received since the start.
    pub fn frames(&self) -> u64 {
        self.shared.frames.load(Ordering::Relaxed)
    }

    /// Why it ended, if it did: reported by ScreenCaptureKit's delegate (sleep, permission revoked,
    /// system stop, or gone display/window), a captured window that closed (its app quit), or a
    /// captured display that went (unplugged).
    pub fn ended(&self) -> Option<String> {
        if let Ok(stopped) = self.shared.stopped.lock()
            && let Some(reason) = stopped.as_ref()
        {
            return Some(reason.clone());
        }
        if let Some(id) = self.display {
            let now = active_displays();
            // An empty answer is a failed question, not every display gone.
            return (!now.is_empty() && !now.contains(&id)).then(|| format!("its display ({id}) went"));
        }
        let id = self.window?;
        (!window_exists(id)).then(|| format!("its window ({id}) closed"))
    }
}

struct HoldGuard<'a>(&'a AtomicBool);

impl Drop for HoldGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn apply_filter_update(
    shared: &Shared,
    trigger: impl FnOnce(RcBlock<dyn Fn(*mut NSError)>),
    timeout: Duration,
) -> bool {
    shared.hold.store(true, Ordering::Release);
    let _release = HoldGuard(&shared.hold);
    let (tx, rx) = mpsc::channel();
    let done = RcBlock::new(move |e: *mut NSError| {
        let _ = tx.send(e.is_null());
    });
    trigger(done);
    rx.recv_timeout(timeout) == Ok(true)
}

/// Looks twice a second for apps `names` names that started (or ended) since, and gives the
/// stream a filter that leaves them out (or, `only`, shows only them); the picture holds until
/// macOS has taken it (or releases on error/timeout). Stops when `PrivacyStop` or stream stop is set.
fn follow_apps(
    stream: SendStream,
    shared: Arc<Shared>,
    display: u32,
    names: Vec<String>,
    only: bool,
    mut hidden: Vec<i32>,
) -> PrivacyStop {
    let flag = Arc::new(AtomicBool::new(false));
    let (wake, wake_rx) = mpsc::channel();
    let stopped = flag.clone();
    let _ = std::thread::Builder::new().name("ferrocast-privacy".into()).spawn(move || {
        let stream = stream;
        loop {
            match wake_rx.recv_timeout(PRIVACY_EVERY) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if stopped.load(Ordering::Acquire) || shared.has_stopped.load(Ordering::Acquire) {
                return;
            }
            // Drain Objective-C autoreleased objects every poll tick on this raw Rust thread
            // (matches OBS `mac-window-capture.m` `@autoreleasepool`), and skip allocating a new
            // `SCContentFilter` when the matching PIDs haven't changed.
            objc2::rc::autoreleasepool(|_| {
                let Ok(c) = content() else { return };
                // SAFETY: the content is live; the stream is live (this thread holds it).
                unsafe {
                    let (apps, pids) = hidden_apps(&c, &names);
                    if pids == hidden || stopped.load(Ordering::Acquire) || shared.has_stopped.load(Ordering::Acquire) {
                        return;
                    }
                    let Some(d) = c.displays().iter().find(|d| d.displayID() == display) else {
                        return;
                    };
                    let none: Retained<NSArray<SCWindow>> = NSArray::new();
                    let filter = if only {
                        SCContentFilter::initWithDisplay_includingApplications_exceptingWindows(
                            SCContentFilter::alloc(),
                            &d,
                            &apps,
                            &none,
                        )
                    } else {
                        SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
                            SCContentFilter::alloc(),
                            &d,
                            &apps,
                            &none,
                        )
                    };
                    if apply_filter_update(
                        &shared,
                        |done| stream.0.updateContentFilter_completionHandler(&filter, Some(&done)),
                        FILTER_WAIT,
                    ) {
                        hidden = pids;
                    }
                }
            });
        }
    });
    PrivacyStop { flag, wake }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        if let Some(stop) = &self.privacy {
            stop.signal();
        }
        let was_stopped = self.shared.has_stopped.swap(true, Ordering::AcqRel);
        self.shared.hold.store(false, Ordering::Release);
        if let Ok(mut latest) = self.shared.latest.lock() {
            *latest = None;
        }
        if let Ok(mut fit) = self.shared.fit.lock() {
            *fit = None;
        }
        // SAFETY: the stream is live; if ScreenCaptureKit already stopped it via delegate we
        // skip `stopCaptureWithCompletionHandler:` completely, otherwise wait briefly so Drop
        // never stalls for seconds after sleep or unplug, then detach outputs cleanly.
        unsafe {
            if let Some(wait) = stop_wait_timeout(was_stopped) {
                let (tx, rx) = mpsc::channel();
                let done = RcBlock::new(move |_e: *mut NSError| {
                    let _ = tx.send(());
                });
                self.stream.stopCaptureWithCompletionHandler(Some(&done));
                let _ = rx.recv_timeout(wait);
            }
            let _ = self
                .stream
                .removeStreamOutput_type_error(ProtocolObject::from_ref(&*self.output), SCStreamOutputType::Screen);
            let _ = self
                .stream
                .removeStreamOutput_type_error(ProtocolObject::from_ref(&*self.output), SCStreamOutputType::Audio);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn a_window_that_isnt_there_reads_as_closed() {
        assert!(!window_exists(0xFFFF_FFF0), "no such window");
        // Every open window: there, and its owner as the window server names it is the app the
        // window list names (how a saved window is told to be still the same app's).
        if let Ok((_, windows)) = shareable() {
            let differ: Vec<_> = windows
                .iter()
                .filter(|w| window_owner(w.id) != Some(Some(w.app.clone())))
                .map(|w| (w.app.clone(), window_owner(w.id)))
                .collect();
            assert!(windows.iter().all(|w| window_exists(w.id)));
            assert!(differ.is_empty(), "names differ: {differ:?}");
        }
        assert_eq!(window_owner(0xFFFF_FFF0), Some(None));
    }

    #[test]
    fn a_display_is_found_by_its_uuid_when_its_number_changed() {
        let now = [(3, Some("AAA".to_string())), (2, Some("BBB".to_string()))];
        // Its UUID wins over its old number, which may be another screen's now.
        assert_eq!(pick_display(1, Some("aaa"), &now), Some(3));
        assert_eq!(pick_display(2, Some("AAA"), &now), Some(3));
        // Its screen isn't connected: nothing, never the other screen.
        assert_eq!(pick_display(3, Some("CCC"), &now), None);
        // Saved before UUIDs: its number while connected, else only a lone display.
        assert_eq!(pick_display(2, None, &now), Some(2));
        assert_eq!(pick_display(1, None, &now), None);
        assert_eq!(pick_display(1, None, &[(5, None)]), Some(5));
        assert_eq!(pick_display(1, Some(""), &[(5, None)]), Some(5));
    }

    #[test]
    fn apps_are_named_by_bundle_id_or_name_in_any_case() {
        let hide = vec!["com.hnc.Discord".to_string(), "mail".to_string()];
        assert!(hides(&hide, "com.hnc.discord", "Discord"));
        assert!(hides(&hide, "com.apple.mail", "Mail"));
        assert!(!hides(&hide, "com.tinyspeck.slackmacgap", "Slack"));
        assert!(!hides(&["".into()], "com.apple.x", ""));
    }

    #[test]
    fn an_app_whose_name_has_invisible_marks_is_still_found() {
        // WhatsApp names itself U+200E then "WhatsApp"; the owner picks "WhatsApp".
        assert!(hides(&["WhatsApp".into()], "net.whatsapp.WhatsApp", "\u{200E}WhatsApp"));
        assert!(hides(&["\u{200E}whatsapp ".into()], "x.y", "WhatsApp"));
        assert!(!hides(&["\u{200E}".into()], "x.y", "\u{200E}"));
    }

    #[test]
    fn a_window_keeps_its_shape_and_fits_the_canvas() {
        // A tall 2x window: its shape kept, made to fit 1920x1080.
        assert_eq!(window_size((2200.0, 2800.0), (1920, 1080)), (849, 1080));
        // A window that fits keeps its own pixels exactly, odd sides too (owner, 12 Nov: 1917
        // asked as 1918 was scaled, its text blurred); never blown up.
        assert_eq!(window_size((801.0, 601.0), (1920, 1080)), (801, 601));
        assert_eq!(window_size((1917.0, 1197.0), (2560, 1440)), (1917, 1197));
        // A wide one fits by its width.
        assert_eq!(window_size((5120.0, 1440.0), (1920, 1080)), (1920, 540));
        // No size known: the canvas.
        assert_eq!(window_size((0.0, 0.0), (1920, 1080)), (1920, 1080));
    }

    /// A BGRA pixel buffer with `rows` rows of `width` pixels, each pixel `bgra`, and a stride
    /// wider than the row (the system pads them). Backed by an IOSurface, like real camera and
    /// screen frames, so `zerocopy::texture` runs in tests too.
    pub(crate) fn bgra_buffer(
        width: usize,
        rows: usize,
        bgra: [u8; 4],
    ) -> Option<CFRetained<objc2_core_video::CVImageBuffer>> {
        use std::ptr::NonNull;
        let attrs = super::super::Dict::new()?;
        let empty = super::super::Dict::new()?;
        // SAFETY: kCVPixelBufferIOSurfacePropertiesKey is a static CoreVideo CFString constant.
        let key: super::super::CFTypeRef =
            unsafe { std::ptr::from_ref(objc2_core_video::kCVPixelBufferIOSurfacePropertiesKey).cast() };
        attrs.set(key, empty.0.0);
        // SAFETY: `attrs.0.0` is a live CFDictionary.
        let attrs_cf: &objc2_core_foundation::CFDictionary = unsafe { &*attrs.0.0.cast() };
        let mut out: *mut objc2_core_video::CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: `attrs_cf` holds IOSurface attributes; `out` is a valid pointer to fill.
        let r = unsafe {
            objc2_core_video::CVPixelBufferCreate(None, width, rows, BGRA, Some(attrs_cf), NonNull::new(&raw mut out)?)
        };
        if r != 0 {
            return None;
        }
        // SAFETY: `out` is a new +1 retained buffer on success; the CFRetained now owns it.
        let buf = unsafe { CFRetained::from_raw(NonNull::new(out)?) };
        // SAFETY: locked for writing (flags 0) around the writes below, then unlocked.
        unsafe { CVPixelBufferLockBaseAddress(&buf, CVPixelBufferLockFlags::empty()) };
        let (stride, base) = (CVPixelBufferGetBytesPerRow(&buf), CVPixelBufferGetBaseAddress(&buf).cast::<u8>());
        for y in 0..rows {
            for x in 0..width {
                // SAFETY: inside the locked buffer's `rows` rows of `stride` bytes.
                unsafe { std::ptr::copy_nonoverlapping(bgra.as_ptr(), base.add(y * stride + x * 4), 4) };
            }
        }
        // SAFETY: locked above.
        unsafe { CVPixelBufferUnlockBaseAddress(&buf, CVPixelBufferLockFlags::empty()) };
        Some(buf)
    }

    /// A frame of the screen goes to the card as it is and comes out in the right colours:
    /// ScreenCaptureKit's BGRA is read as red, green, blue, with no swap of ours.
    #[test]
    fn a_screen_frame_reaches_the_card_in_the_right_colours() {
        use crate::{Content, Item, Placement, Scene};
        let Ok(gpu) = crate::Compositor::new() else { return };
        // Blue 10, green 120, red 250, opaque: width 70 (stride padded past 280 bytes).
        let Some(buf) = bgra_buffer(70, 5, [10, 120, 250, 255]) else { return };
        let surface = Surface::of(buf).unwrap_or_else(|| panic!("a BGRA buffer is taken"));
        assert_eq!(surface.size(), (70, 5));
        let scene = Scene {
            width: 70,
            height: 5,
            items: vec![Item {
                content: Content::Uploaded { id: 3, width: 70, height: 5, premultiplied: true },
                ..Item::picture(Picture::solid(1, 1, [0; 4]), Placement::default())
            }],
        };
        // Twice, and a third time after a frame is drawn (kept buffers, then the plain way).
        for _ in 0..3 {
            gpu.upload_surface(3, &surface).unwrap_or_else(|e| panic!("{e}"));
            let px = gpu.render(&scene).unwrap_or_default();
            let at = |x: usize, y: usize| px[(y * 70 + x) * 4..(y * 70 + x) * 4 + 4].to_vec();
            assert_eq!(at(0, 0), vec![250, 120, 10, 255]);
            assert_eq!(at(69, 4), vec![250, 120, 10, 255]);
        }
        // The same frame as an RGBA picture (the other way to take it) agrees.
        let p = surface.to_picture().unwrap_or_else(|| panic!("a picture"));
        assert_eq!(&p.rgba[..4], &[250, 120, 10, 255]);
    }

    fn pixel_buffer(width: usize, rows: usize, format: u32) -> Option<CFRetained<objc2_core_video::CVImageBuffer>> {
        use std::ptr::NonNull;
        let mut out: *mut objc2_core_video::CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: creating a plain pixel buffer for tests; `out` is valid to fill.
        let r = unsafe {
            objc2_core_video::CVPixelBufferCreate(None, width, rows, format, None, NonNull::new(&raw mut out)?)
        };
        if r != 0 {
            return None;
        }
        // SAFETY: `out` is a new +1 retained buffer on success; CFRetained owns it.
        unsafe { Some(CFRetained::from_raw(NonNull::new(out)?)) }
    }

    #[test]
    fn copy_bgra_rejects_non_bgra_and_planar_buffers() {
        use objc2_core_video::{
            kCVPixelFormatType_32ARGB, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVPixelFormatType_422YpCbCr8,
        };
        // 32ARGB: same 4-bpp stride as BGRA, must not be misread as BGRA.
        let Some(argb) = pixel_buffer(8, 4, kCVPixelFormatType_32ARGB) else { return };
        assert!(copy_bgra(&argb).is_none(), "32ARGB must be rejected by copy_bgra");
        // Packed 4:2:2 UYVY (2vuy): 2-bpp stride, must be rejected.
        let Some(uyvy) = pixel_buffer(8, 4, kCVPixelFormatType_422YpCbCr8) else { return };
        assert!(copy_bgra(&uyvy).is_none(), "2vuy must be rejected by copy_bgra");
        // Planar NV12 (420v): planar buffer must be rejected by copy_bgra.
        let Some(nv12) = pixel_buffer(8, 4, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange) else { return };
        assert!(copy_bgra(&nv12).is_none(), "420v planar buffer must be rejected by copy_bgra");
    }

    pub(crate) fn buffer_retain_count(buf: &objc2_core_video::CVImageBuffer) -> isize {
        // SAFETY: reading CF retain count on a live CVImageBuffer.
        unsafe { super::super::CFGetRetainCount(std::ptr::from_ref(buf).cast()) }
    }

    #[test]
    fn copy_video_frame_converts_yuv_and_argb_safely() {
        use objc2_core_video::{
            CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, kCVPixelFormatType_1Monochrome,
            kCVPixelFormatType_32ARGB, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVPixelFormatType_422YpCbCr8,
        };
        // 32ARGB: [A, R, G, B] -> [R, G, B, A].
        let Some(argb) = pixel_buffer(4, 2, kCVPixelFormatType_32ARGB) else { return };
        // SAFETY: locking freshly created test pixel buffer for writing.
        unsafe { CVPixelBufferLockBaseAddress(&argb, CVPixelBufferLockFlags::empty()) };
        let (stride, base) = (CVPixelBufferGetBytesPerRow(&argb), CVPixelBufferGetBaseAddress(&argb).cast::<u8>());
        for y in 0..2 {
            for x in 0..4 {
                // SAFETY: inside locked 4x2 ARGB pixel buffer.
                unsafe {
                    std::ptr::copy_nonoverlapping([255u8, 200, 100, 50].as_ptr(), base.add(y * stride + x * 4), 4)
                };
            }
        }
        // SAFETY: locked above.
        unsafe { CVPixelBufferUnlockBaseAddress(&argb, CVPixelBufferLockFlags::empty()) };
        let pic = copy_video_frame(&argb, true).unwrap_or_else(|| panic!("32ARGB conversion"));
        assert_eq!(&pic.rgba[..4], &[200, 100, 50, 255]);

        // Bi-planar 420v (NV12) BT.709 TV white (Y=235, Cb=128, Cr=128).
        let Some(nv12) = pixel_buffer(4, 2, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange) else { return };
        // SAFETY: locking freshly created planar buffer for writing.
        unsafe { CVPixelBufferLockBaseAddress(&nv12, CVPixelBufferLockFlags::empty()) };
        let y_stride = CVPixelBufferGetBytesPerRowOfPlane(&nv12, 0);
        let y_base = CVPixelBufferGetBaseAddressOfPlane(&nv12, 0).cast::<u8>();
        let uv_base = CVPixelBufferGetBaseAddressOfPlane(&nv12, 1).cast::<u8>();
        for row in 0..2 {
            // SAFETY: plane 0 has 2 rows of 4 Y bytes.
            unsafe { std::ptr::write_bytes(y_base.add(row * y_stride), 235, 4) };
        }
        for x in 0..2 {
            // SAFETY: plane 1 has 1 row of 2 [Cb, Cr] pairs.
            unsafe { std::ptr::copy_nonoverlapping([128u8, 128].as_ptr(), uv_base.add(x * 2), 2) };
        }
        // SAFETY: locked above.
        unsafe { CVPixelBufferUnlockBaseAddress(&nv12, CVPixelBufferLockFlags::empty()) };
        let pic = copy_video_frame(&nv12, true).unwrap_or_else(|| panic!("420v conversion"));
        assert_eq!(pic.rgba[..4], [255, 255, 255, 255]);

        // Packed 4:2:2 UYVY (2vuy) BT.709 TV white ([128, 235, 128, 235]).
        let Some(uyvy) = pixel_buffer(4, 2, kCVPixelFormatType_422YpCbCr8) else { return };
        // SAFETY: locking test pixel buffer for writing.
        unsafe { CVPixelBufferLockBaseAddress(&uyvy, CVPixelBufferLockFlags::empty()) };
        let (stride, base) = (CVPixelBufferGetBytesPerRow(&uyvy), CVPixelBufferGetBaseAddress(&uyvy).cast::<u8>());
        for y in 0..2 {
            for x in 0..2 {
                // SAFETY: inside locked 4x2 2vuy pixel buffer (2 macropixels per row).
                unsafe {
                    std::ptr::copy_nonoverlapping([128u8, 235, 128, 235].as_ptr(), base.add(y * stride + x * 4), 4);
                }
            }
        }
        // SAFETY: locked above.
        unsafe { CVPixelBufferUnlockBaseAddress(&uyvy, CVPixelBufferLockFlags::empty()) };
        let pic = copy_video_frame(&uyvy, true).unwrap_or_else(|| panic!("2vuy conversion"));
        assert_eq!(pic.rgba[..4], [255, 255, 255, 255]);

        // Unsupported formats return None cleanly instead of misreading memory.
        let Some(mono) = pixel_buffer(4, 2, kCVPixelFormatType_1Monochrome) else { return };
        assert!(copy_video_frame(&mono, true).is_none());
    }

    #[test]
    fn stream_delegate_error_marks_capture_ended_for_sleep_permission_or_gone() {
        let shared = Arc::new(Shared::default());
        shared.hold.store(true, Ordering::Release);
        let output = Output::new(shared.clone());
        // Output responds to SCStreamDelegate selector `stream:didStopWithError:`.
        // SAFETY: calling NSObject's `respondsToSelector:` on a live `Output`.
        let responds: bool = unsafe { msg_send![&*output, respondsToSelector: objc2::sel!(stream:didStopWithError:)] };
        assert!(responds, "Output must implement SCStreamDelegate::stream:didStopWithError:");

        let perm = format_stream_error(SCStreamErrorCode::UserDeclined.0, "user declined");
        assert!(perm.contains("permission"));
        let gone = format_stream_error(SCStreamErrorCode::NoCaptureSource.0, "no display");
        assert!(gone.contains("gone"));
        let sleep_reason = format_stream_error(SCStreamErrorCode::SystemStoppedStream.0, "system stopped");
        assert!(sleep_reason.contains("stopped"));

        record_stream_error(&shared, sleep_reason.clone());
        assert!(shared.has_stopped.load(Ordering::Acquire));
        assert!(!shared.hold.load(Ordering::Acquire), "delegate error must release hold");
        assert_eq!(shared.stopped.lock().ok().and_then(|s| s.clone()), Some(sleep_reason));
    }

    #[test]
    fn follow_apps_clears_hold_on_error_and_timeout() {
        let shared = Arc::new(Shared::default());
        let domain = objc2_foundation::NSString::from_str("SCStreamErrorDomain");
        // SAFETY: creating a test NSError with valid NSString domain.
        let err = unsafe { NSError::errorWithDomain_code_userInfo(&domain, -3808, None) };

        let ok = apply_filter_update(
            &shared,
            |done| done.call((Retained::as_ptr(&err).cast_mut(),)),
            Duration::from_millis(200),
        );
        assert!(!ok);
        assert!(!shared.hold.load(Ordering::Acquire), "error must clear hold");

        let ok = apply_filter_update(&shared, |_done| {}, Duration::from_millis(15));
        assert!(!ok);
        assert!(!shared.hold.load(Ordering::Acquire), "timeout must clear hold");

        let ok = apply_filter_update(&shared, |done| done.call((std::ptr::null_mut(),)), Duration::from_millis(200));
        assert!(ok);
        assert!(!shared.hold.load(Ordering::Acquire));
    }

    #[test]
    fn screen_drop_wait_is_bounded_and_skips_when_already_stopped() {
        assert_eq!(stop_wait_timeout(true), None);
        let wait = stop_wait_timeout(false);
        assert!(wait.is_some_and(|d| d <= Duration::from_millis(500)));
    }
}
