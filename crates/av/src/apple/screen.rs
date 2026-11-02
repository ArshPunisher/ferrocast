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
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
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
    // SAFETY: building a filter from the live content's display and apps.
    unsafe {
        let d: Retained<SCDisplay> = c
            .displays()
            .iter()
            .find(|d| d.displayID() == id)
            .ok_or_else(|| AvError::Source(format!("display {id} isn't connected")))?;
        let none: Retained<NSArray<SCWindow>> = NSArray::new();
        let (apps, pids) = hidden_apps(c, hide);
        let filter = SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
            SCContentFilter::alloc(),
            &d,
            &apps,
            &none,
        );
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

/// What can be captured now: displays, and windows that are on screen and have a title.
pub fn shareable() -> Result<(Vec<Display>, Vec<Window>), AvError> {
    let c = content()?;
    // SAFETY: reading properties of the live content object.
    unsafe {
        let displays = c
            .displays()
            .iter()
            .map(|d| Display { id: d.displayID(), width: d.width() as u32, height: d.height() as u32 })
            .collect();
        let windows = c
            .windows()
            .iter()
            .filter_map(|w| {
                let title = w.title()?.to_string();
                if title.is_empty() {
                    return None;
                }
                let app = w.owningApplication().map(|a| a.applicationName().to_string()).unwrap_or_default();
                Some(Window { id: w.windowID(), title, app })
            })
            .collect();
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
    latest: Mutex<Option<Picture>>,
    frames: AtomicU64,
    sound: Mutex<std::collections::VecDeque<(Vec<f32>, u64)>>,
    /// The picture holds (no new frame kept) while an app to leave out isn't left out yet.
    hold: AtomicBool,
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
            // SAFETY: the sample buffer is live for this call; an idle frame has no image.
            let Some(image) = (unsafe { sample.image_buffer() }) else { return };
            if let Some(p) = copy_bgra(&image) {
                if let Ok(mut l) = self.ivars().latest.lock() {
                    *l = Some(p);
                }
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
        let mut rgba = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            // SAFETY: the locked buffer holds `h` rows of `stride` bytes at `base`.
            let row = unsafe { std::slice::from_raw_parts(base.add(y * stride), w * 4) };
            for px in row.as_chunks::<4>().0 {
                rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
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
        let c = content()?;
        // SAFETY: building ScreenCaptureKit objects from the live content's displays and
        // windows, on this thread; each call gets live arguments.
        unsafe {
            let mut hidden = Vec::new();
            let filter = match target {
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
            let config = SCStreamConfiguration::new();
            config.setWidth(width.clamp(16, 8192) as usize);
            config.setHeight(height.clamp(16, 8192) as usize);
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
            let privacy = match target {
                Target::Display(id) if !hide.is_empty() => {
                    Some(keep_apps_out(SendStream(stream.clone()), shared.clone(), id, hide.to_vec(), hidden))
                }
                _ => None,
            };
            Ok(Self { stream, output, _queue: queue, shared, privacy })
        }
    }

    /// The newest frame (taken: the next call gives the one after, or None until it comes).
    pub fn take(&self) -> Option<Picture> {
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

/// Looks twice a second for apps `hide` names that started (or ended) since, and gives the
/// stream a filter that leaves them out; the picture holds until macOS has taken it (and
/// keeps holding, trying again, if it refuses). Stops when the flag is set.
fn keep_apps_out(
    stream: SendStream,
    shared: Arc<Shared>,
    display: u32,
    hide: Vec<String>,
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
                let Ok((filter, pids)) = display_filter(&c, display, &hide) else { continue };
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
}
