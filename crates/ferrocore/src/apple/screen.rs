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
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCRunningApplication, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamOutput, SCStreamOutputType, SCWindow,
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
    hide.iter().any(|h| h.eq_ignore_ascii_case(bundle) || (!name.is_empty() && h.eq_ignore_ascii_case(name)))
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
        let d: Retained<SCDisplay> = c
            .displays()
            .iter()
            .find(|d| d.displayID() == id)
            .ok_or_else(|| AvError::Source(format!("display {id} isn't connected")))?;
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

/// How long ScreenCaptureKit gets to answer.
const WAIT: Duration = Duration::from_secs(10);
/// kCVPixelFormatType_32BGRA.
const BGRA: u32 = u32::from_be_bytes(*b"BGRA");

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
                let app = owner.applicationName().to_string();
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
            .map(|a| App { id: a.bundleIdentifier().to_string(), name: a.applicationName().to_string() })
            .filter(|a| !a.id.is_empty())
            .collect()
    };
    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));
    apps.dedup_by(|a, b| a.id == b.id);
    Ok(apps)
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
        if want.0.abs_diff(fit.size.0) <= 2 && want.1.abs_diff(fit.size.1) <= 2 {
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
            if self.ivars().hold.load(Ordering::Relaxed) {
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

/// A window's capture size: its pixels, made smaller (never larger) to fit in `max` with its
/// shape kept, both sides even (the encoders' NV12 needs it).
fn window_size((w, h): (f64, f64), max: (u32, u32)) -> (u32, u32) {
    if !(w >= 1.0 && h >= 1.0) {
        return max;
    }
    let f = (f64::from(max.0) / w).min(f64::from(max.1) / h).min(1.0);
    let even = |v: f64| ((v * f / 2.0).round() as u32).max(8) * 2;
    (even(w), even(h))
}

/// A frame of a display as the system holds it: BGRA, colours multiplied by alpha, not copied.
/// It is kept (so ScreenCaptureKit can't reuse its memory) until it has been sent to the card.
/// Cloning keeps the same buffer one more time (no pixels are copied).
#[derive(Clone)]
pub struct Surface {
    image: CFRetained<objc2_core_video::CVImageBuffer>,
    width: u32,
    height: u32,
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
    /// None for a buffer that isn't 32-bit BGRA or has no size.
    pub(crate) fn of(image: CFRetained<objc2_core_video::CVImageBuffer>) -> Option<Self> {
        let (w, h) = (CVPixelBufferGetWidth(&image), CVPixelBufferGetHeight(&image));
        if w == 0 || h == 0 || CVPixelBufferGetPixelFormatType(&image) != BGRA {
            return None;
        }
        Some(Self { image, width: u32::try_from(w).ok()?, height: u32::try_from(h).ok()? })
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
        // SAFETY: locking a live pixel buffer read-only; unlocked below with the same flag.
        if unsafe { CVPixelBufferLockBaseAddress(&self.image, CVPixelBufferLockFlags::ReadOnly) } != 0 {
            return None;
        }
        let stride = CVPixelBufferGetBytesPerRow(&self.image);
        let base = CVPixelBufferGetBaseAddress(&self.image).cast::<u8>();
        let h = self.height as usize;
        let out = if base.is_null() || stride < self.width as usize * 4 {
            None
        } else {
            // SAFETY: the locked buffer holds `h` rows of `stride` bytes at `base`, and nothing
            // writes to it while it is locked for reading.
            Some(f(unsafe { std::slice::from_raw_parts(base, stride * h) }, stride))
        };
        // SAFETY: locked above.
        unsafe { CVPixelBufferUnlockBaseAddress(&self.image, CVPixelBufferLockFlags::ReadOnly) };
        out
    }

    /// The frame as an RGBA picture (a copy, for the ways that need one).
    pub fn to_picture(&self) -> Option<Picture> {
        copy_bgra(&self.image)
    }
}

/// A BGRA pixel buffer's pixels as RGBA rows (premultiplied, as ScreenCaptureKit gives them).
pub(crate) fn copy_bgra(image: &objc2_core_video::CVPixelBuffer) -> Option<Picture> {
    // SAFETY: locking a live pixel buffer read-only; unlocked below with the same flag.
    if unsafe { CVPixelBufferLockBaseAddress(image, CVPixelBufferLockFlags::ReadOnly) } != 0 {
        return None;
    }
    let (w, h, stride) =
        (CVPixelBufferGetWidth(image), CVPixelBufferGetHeight(image), CVPixelBufferGetBytesPerRow(image));
    let base = CVPixelBufferGetBaseAddress(image).cast::<u8>();
    let mut out = None;
    if !base.is_null() && w > 0 && h > 0 && stride >= w * 4 {
        // Each pixel is turned as one 32-bit word (blue and red swapped) so the loop compiles to
        // wide vector moves; a byte-by-byte `extend_from_slice` per pixel was the biggest CPU
        // cost in a 1080p recording.
        let mut rgba = vec![0u8; w * h * 4];
        for (y, out) in rgba.chunks_exact_mut(w * 4).enumerate() {
            // SAFETY: the locked buffer holds `h` rows of `stride` bytes at `base`.
            let row = unsafe { std::slice::from_raw_parts(base.add(y * stride), w * 4) };
            for (d, s) in out.as_chunks_mut::<4>().0.iter_mut().zip(row.as_chunks::<4>().0) {
                let v = u32::from_le_bytes(*s);
                *d = ((v & 0xFF00_FF00) | (v << 16 & 0x00FF_0000) | (v >> 16 & 0x0000_00FF)).to_le_bytes();
            }
        }
        out = Some(Picture { width: w as u32, height: h as u32, rgba, premultiplied: true });
    }
    // SAFETY: locked above.
    unsafe { CVPixelBufferUnlockBaseAddress(image, CVPixelBufferLockFlags::ReadOnly) };
    out
}

/// A running capture; dropping it stops.
pub struct ScreenCapture {
    stream: Retained<SCStream>,
    output: Retained<Output>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
    shared: Arc<Shared>,
    /// Tells the thread that keeps apps out (when there is one) to stop.
    privacy: Option<Arc<AtomicBool>>,
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
                    let w: Retained<SCWindow> = c
                        .windows()
                        .iter()
                        .find(|w| w.windowID() == id)
                        .ok_or_else(|| AvError::Source(format!("window {id} is gone")))?;
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
            let stream = SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None);
            let shared = Arc::new(Shared::default());
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
            let output = Output::new(shared.clone());
            let queue = DispatchQueue::new("ferrocast.screen", DispatchQueueAttr::SERIAL);
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|e| AvError::Source(format!("the capture couldn't start ({})", e.localizedDescription())))?;
            if sound {
                stream
                    .addStreamOutput_type_sampleHandlerQueue_error(
                        ProtocolObject::from_ref(&*output),
                        SCStreamOutputType::Audio,
                        Some(&queue),
                    )
                    .map_err(|e| {
                        AvError::Source(format!("the Mac's sound couldn't be captured ({})", e.localizedDescription()))
                    })?;
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
                Ok(Err(m)) => return Err(AvError::Source(format!("the capture couldn't start ({m})"))),
                Err(_) => return Err(AvError::Source("macOS didn't start the capture".into())),
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
            Ok(Self { stream, output, _queue: queue, shared, privacy })
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

    /// Frames received since the start.
    pub fn frames(&self) -> u64 {
        self.shared.frames.load(Ordering::Relaxed)
    }

    /// Why it ended, if it did (ScreenCaptureKit's stream doesn't end by itself here).
    pub fn ended(&self) -> Option<String> {
        None
    }
}

/// Looks twice a second for apps `names` names that started (or ended) since, and gives the
/// stream a filter that leaves them out (or, `only`, shows only them); the picture holds until
/// macOS has taken it (and keeps holding, trying again, if it refuses). Stops when the flag is
/// set.
fn follow_apps(
    stream: SendStream,
    shared: Arc<Shared>,
    display: u32,
    names: Vec<String>,
    only: bool,
    mut hidden: Vec<i32>,
) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let _ = std::thread::Builder::new().name("ferrocast-privacy".into()).spawn(move || {
        let stream = stream;
        loop {
            std::thread::sleep(PRIVACY_EVERY);
            if stopped.load(Ordering::Relaxed) {
                return;
            }
            let Ok(c) = content() else { continue };
            // SAFETY: the content is live; the stream is live (this thread holds it).
            unsafe {
                let Ok((filter, pids)) = apps_filter(&c, display, &names, only) else { continue };
                if pids == hidden || stopped.load(Ordering::Relaxed) {
                    continue;
                }
                shared.hold.store(true, Ordering::Relaxed);
                let (tx, rx) = mpsc::channel();
                let done = RcBlock::new(move |e: *mut NSError| {
                    let _ = tx.send(e.is_null());
                });
                stream.0.updateContentFilter_completionHandler(&filter, Some(&done));
                if rx.recv_timeout(WAIT) == Ok(true) {
                    hidden = pids;
                    shared.hold.store(false, Ordering::Relaxed);
                }
            }
        }
    });
    stop
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        if let Some(stop) = &self.privacy {
            stop.store(true, Ordering::Relaxed);
        }
        let (tx, rx) = mpsc::channel();
        let done = RcBlock::new(move |_e: *mut NSError| {
            let _ = tx.send(());
        });
        // SAFETY: the stream is live; stopping waits for ScreenCaptureKit (briefly) so no frame
        // arrives at an output that's going.
        unsafe {
            self.stream.stopCaptureWithCompletionHandler(Some(&done));
            let _ = rx.recv_timeout(WAIT);
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
mod tests {
    use super::*;

    #[test]
    fn apps_are_named_by_bundle_id_or_name_in_any_case() {
        let hide = vec!["com.hnc.Discord".to_string(), "mail".to_string()];
        assert!(hides(&hide, "com.hnc.discord", "Discord"));
        assert!(hides(&hide, "com.apple.mail", "Mail"));
        assert!(!hides(&hide, "com.tinyspeck.slackmacgap", "Slack"));
        assert!(!hides(&["".into()], "com.apple.x", ""));
    }

    #[test]
    fn a_window_keeps_its_shape_and_fits_the_canvas() {
        // A tall 2x window: its shape kept, made to fit 1920x1080, sides even.
        assert_eq!(window_size((2200.0, 2800.0), (1920, 1080)), (848, 1080));
        // A small window keeps its own pixels (never blown up).
        assert_eq!(window_size((801.0, 601.0), (1920, 1080)), (802, 602));
        // A wide one fits by its width.
        assert_eq!(window_size((5120.0, 1440.0), (1920, 1080)), (1920, 540));
        // No size known: the canvas.
        assert_eq!(window_size((0.0, 0.0), (1920, 1080)), (1920, 1080));
    }

    /// A BGRA pixel buffer with `rows` rows of `width` pixels, each pixel `bgra`, and a stride
    /// wider than the row (the system pads them).
    fn bgra_buffer(width: usize, rows: usize, bgra: [u8; 4]) -> Option<CFRetained<objc2_core_video::CVImageBuffer>> {
        use std::ptr::NonNull;
        let mut out: *mut objc2_core_video::CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: a new buffer asked for with no attributes; `out` is a valid pointer to fill.
        let r = unsafe {
            objc2_core_video::CVPixelBufferCreate(None, width, rows, BGRA, None, NonNull::new(&raw mut out)?)
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
}
