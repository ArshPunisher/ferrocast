//! Cameras and microphones with AVFoundation: a device's frames (BGRA, the newest kept) or its
//! sound (stereo float at 48 kHz, every piece kept in order with its time). Times are the Mac's
//! host clock in nanoseconds, the same as screen capture's, so everything lines up.

use std::collections::{HashSet, VecDeque};
use std::ffi::{CStr, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send, sel};
use objc2_av_foundation::{
    AVCaptureAudioDataOutput, AVCaptureAudioDataOutputSampleBufferDelegate, AVCaptureConnection, AVCaptureDevice,
    AVCaptureDeviceDiscoverySession, AVCaptureDeviceInput, AVCaptureDevicePosition, AVCaptureDeviceType,
    AVCaptureDeviceTypeBuiltInWideAngleCamera, AVCaptureDeviceTypeDeskViewCamera,
    AVCaptureDeviceWasConnectedNotification, AVCaptureDeviceWasDisconnectedNotification, AVCaptureOutput,
    AVCaptureSession, AVCaptureSessionDidStopRunningNotification, AVCaptureSessionErrorKey,
    AVCaptureSessionInterruptionEndedNotification, AVCaptureSessionPresetInputPriority,
    AVCaptureSessionRuntimeErrorNotification, AVCaptureSessionWasInterruptedNotification, AVCaptureVideoDataOutput,
    AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeAudio, AVMediaTypeMuxed, AVMediaTypeVideo,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    CMSampleBuffer, CMTime, CMVideoFormatDescriptionGetDimensions, kCMFormatDescriptionExtension_YCbCrMatrix,
    kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2,
};
use objc2_core_video::{
    CVPixelBufferGetHeight, CVPixelBufferGetWidth, kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32ARGB,
    kCVPixelFormatType_32BGRA, kCVPixelFormatType_32RGBA, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVPixelFormatType_420YpCbCr10BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange, kCVPixelFormatType_422YpCbCr8,
    kCVPixelFormatType_422YpCbCr8_yuvs, kCVPixelFormatType_422YpCbCr8FullRange,
};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNotification, NSNotificationCenter, NSNumber, NSString};

use crate::engine::CameraSpec;
use crate::{AvError, Picture};

/// A camera or microphone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// Stable between runs (what a scene saves).
    pub id: String,
    pub name: String,
}

/// Sound kept before it's taken (a stalled reader can't grow memory): about 5 s.
const MAX_SOUND_PIECES: usize = 500;

/// Looks up an AVFoundation string constant (e.g. macOS 14+ Continuity Camera / External device
/// types) at runtime so macOS 13 deployment targets keep launching cleanly.
fn dynamic_device_type(symbol: &CStr) -> Option<&'static AVCaptureDeviceType> {
    // SAFETY: `dlsym` resolves an exported C symbol `const AVCaptureDeviceType *`; we verify both
    // the symbol pointer and its pointed Objective-C NSString constant pointer are non-null.
    unsafe {
        let ptr = libc::dlsym(libc::RTLD_DEFAULT, symbol.as_ptr()) as *const *const AVCaptureDeviceType;
        if ptr.is_null() {
            return None;
        }
        let val = *ptr;
        (!val.is_null()).then(|| &*val)
    }
}

fn camera_device_types() -> Vec<&'static AVCaptureDeviceType> {
    // SAFETY: static AVFoundation constants + optional runtime Continuity/External device types.
    unsafe {
        let mut device_types: Vec<&AVCaptureDeviceType> =
            vec![AVCaptureDeviceTypeBuiltInWideAngleCamera, AVCaptureDeviceTypeDeskViewCamera];
        for sym in [
            c"AVCaptureDeviceTypeExternalUnknown",
            c"AVCaptureDeviceTypeExternal",
            c"AVCaptureDeviceTypeContinuityCamera",
        ] {
            if let Some(dt) = dynamic_device_type(sym) {
                device_types.push(dt);
            }
        }
        device_types
    }
}

fn camera_discovery_sessions() -> Vec<Retained<AVCaptureDeviceDiscoverySession>> {
    // SAFETY: querying AVFoundation discovery sessions with valid constant device types.
    unsafe {
        let types = NSArray::from_slice(&camera_device_types());
        [AVMediaTypeVideo, AVMediaTypeMuxed]
            .into_iter()
            .flatten()
            .map(|kind| {
                AVCaptureDeviceDiscoverySession::discoverySessionWithDeviceTypes_mediaType_position(
                    &types,
                    Some(kind),
                    AVCaptureDevicePosition::Unspecified,
                )
            })
            .collect()
    }
}

fn discover_cameras() -> Vec<Device> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    // SAFETY: reading `.devices()` and device metadata from live discovery sessions.
    unsafe {
        for session in camera_discovery_sessions() {
            for d in session.devices().iter() {
                let id = d.uniqueID().to_string();
                if seen.insert(id.clone()) {
                    out.push(Device { id, name: d.localizedName().to_string() });
                }
            }
        }
    }
    out
}

fn list(video: bool) -> Vec<Device> {
    if video {
        return discover_cameras();
    }
    // SAFETY: AVFoundation's constant media types; listing audio devices has no other requirement.
    unsafe {
        let Some(kind) = AVMediaTypeAudio else { return Vec::new() };
        #[allow(deprecated)]
        AVCaptureDevice::devicesWithMediaType(kind)
            .iter()
            .map(|d| Device { id: d.uniqueID().to_string(), name: d.localizedName().to_string() })
            .collect()
    }
}

/// The system's microphone now (System Settings → Sound → Input), by its unique id.
pub fn default_microphone() -> Option<String> {
    // SAFETY: AVFoundation's constant media type; asking for the default device.
    unsafe {
        let kind = AVMediaTypeAudio?;
        AVCaptureDevice::defaultDeviceWithMediaType(kind).map(|d| d.uniqueID().to_string())
    }
}

/// Whether a microphone id means "the system's" (OBS's "default"), which follows it.
fn is_default(id: &str) -> bool {
    id.is_empty() || id == "default"
}

/// Cameras connected now.
pub fn cameras() -> Vec<Device> {
    ensure_hotplug_listener();
    list(true)
}

/// Microphones and other sound inputs connected now.
pub fn microphones() -> Vec<Device> {
    ensure_hotplug_listener();
    list(false)
}

/// Monotonic counter bumped whenever any camera or microphone connects, disconnects, or changes
/// in Continuity Camera discovery so device option caches refresh immediately.
pub fn hotplug_epoch() -> u64 {
    ensure_hotplug_listener();
    HOTPLUG_EPOCH.load(Ordering::Acquire)
}

/// What a capture's delegate keeps.
#[derive(Default)]
struct Shared {
    device_id: String,
    label: &'static str,
    /// The "staying off the Bluetooth microphone" line has been logged.
    bluetooth_noted: AtomicBool,
    /// Set by `AVCaptureSessionWasInterruptedNotification` and cleared on `InterruptionEnded`.
    interrupted: AtomicBool,
    /// Suppresses stop notifications triggered intentionally during `Capture::drop`.
    dropping: AtomicBool,
    /// Something to say on the dashboard about how this capture opened (taken once).
    pending_notice: Mutex<Option<crate::notices::Notice>>,
    frame: Mutex<Option<(Picture, u64)>>,
    /// The newest frame as the camera holds it (no copy), when the card can read it directly.
    surface: Mutex<Option<(super::screen::Surface, u64)>>,
    sound: Mutex<VecDeque<(Vec<f32>, u64)>>,
    count: AtomicU64,
    /// Why AVFoundation reported this capture ended (device disconnected, stopped running, or error).
    stopped: Mutex<Option<String>>,
}

/// Process-wide hotplug sequence + recently connected unique device ids for `engine::watchdog`.
static HOTPLUG_SEQ: AtomicU64 = AtomicU64::new(0);
static HOTPLUG_EPOCH: AtomicU64 = AtomicU64::new(1);
static HOTPLUG_RECENT: Mutex<VecDeque<(u64, String)>> = Mutex::new(VecDeque::new());
const MAX_HOTPLUG_RECENT: usize = 64;

fn bump_hotplug_epoch() {
    HOTPLUG_EPOCH.fetch_add(1, Ordering::AcqRel);
}

fn record_hotplug_connected(id: String) {
    let seq = HOTPLUG_SEQ.fetch_add(1, Ordering::AcqRel) + 1;
    bump_hotplug_epoch();
    if let Ok(mut q) = HOTPLUG_RECENT.lock() {
        q.push_back((seq, id));
        while q.len() > MAX_HOTPLUG_RECENT {
            q.pop_front();
        }
    }
}

fn record_hotplug_disconnected(id: &str) {
    clear_own(id);
    bump_hotplug_epoch();
}

fn diff_device_sets(prev: &HashSet<String>, next: &HashSet<String>) -> (Vec<String>, Vec<String>) {
    let mut added: Vec<String> = next.iter().filter(|id| !prev.contains(*id)).cloned().collect();
    let mut removed: Vec<String> = prev.iter().filter(|id| !next.contains(*id)).cloned().collect();
    added.sort();
    removed.sort();
    (added, removed)
}

fn clear_own(id: &str) {
    if let Ok(mut all) = OWN.lock()
        && let Some(map) = all.as_mut()
    {
        map.remove(id);
    }
}

fn handle_disconnect(shared: &Shared, id: &str) {
    clear_own(id);
    if id != shared.device_id {
        return;
    }
    record_stopped(shared, format!("the {} was disconnected", shared.label));
}

fn record_stopped(shared: &Shared, reason: String) {
    if let Ok(mut stopped) = shared.stopped.lock()
        && stopped.is_none()
    {
        *stopped = Some(reason);
    }
}

fn runtime_error_detail(note: &NSNotification) -> String {
    // SAFETY: `AVCaptureSessionErrorKey` is a static immutable Foundation NSString constant.
    let key: &AnyObject = unsafe { AVCaptureSessionErrorKey }.as_ref();
    note.userInfo()
        .and_then(|u| u.objectForKey(key))
        .and_then(|v| v.downcast::<NSError>().ok())
        .map(|e| e.localizedDescription().to_string())
        .unwrap_or_else(|| "capture session error".into())
}

/// True when the sample's colorimetry is BT.709 (or falls back to BT.709 for HD >= 720p).
fn is_bt709(sample: &CMSampleBuffer, image: &objc2_core_video::CVImageBuffer) -> bool {
    // SAFETY: reading live CMSampleBuffer format description extension; falls back to resolution check.
    unsafe {
        if let Some(desc) = sample.format_description()
            && let Some(ext) = desc.extension(kCMFormatDescriptionExtension_YCbCrMatrix)
        {
            let ext_obj: &AnyObject = &*(CFRetained::as_ptr(&ext).as_ptr().cast());
            let bt709_obj: &AnyObject = &*(std::ptr::from_ref(kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2).cast());
            return msg_send![ext_obj, isEqual: bt709_obj];
        }
    }
    CVPixelBufferGetWidth(image) >= 1280 || CVPixelBufferGetHeight(image) >= 720
}

/// A sample buffer's presentation time on the host clock, nanoseconds.
fn time_ns(sample: &CMSampleBuffer) -> u64 {
    // SAFETY: reading a live sample buffer's time.
    let t = unsafe { sample.presentation_time_stamp() };
    if t.timescale <= 0 || t.value < 0 {
        return 0;
    }
    (i128::from(t.value) * 1_000_000_000 / i128::from(t.timescale)) as u64
}

define_class!(
    // SAFETY: NSObject may be subclassed; the ivars are an Arc (dropped with the object).
    #[unsafe(super(NSObject))]
    #[name = "FerrocastCaptureOutput"]
    #[ivars = Arc<Shared>]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for Output {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(&self, output: &AVCaptureOutput, sample: &CMSampleBuffer, _conn: &AVCaptureConnection) {
            let ts = time_ns(sample);
            // Video frames carry an image; sound pieces don't.
            // SAFETY: the sample buffer is live for this call.
            if let Some(image) = unsafe { sample.image_buffer() } {
                // Kept as the camera holds it and handed to the card with no copy (as a screen's
                // frames are); non-BGRA or non-zerocopy frames convert safely via copy_video_frame.
                if super::zerocopy::enabled()
                    && let Some(s) = super::screen::Surface::of(image.clone())
                {
                    if let Ok(mut f) = self.ivars().surface.lock() {
                        *f = Some((s, ts));
                    }
                    self.ivars().count.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                let bt709 = is_bt709(sample, &image);
                if let Some(p) = super::screen::copy_video_frame(&image, bt709) {
                    if let Ok(mut f) = self.ivars().frame.lock() {
                        *f = Some((p, ts));
                    }
                    self.ivars().count.fetch_add(1, Ordering::Relaxed);
                }
                return;
            }
            let _ = output;
            // SAFETY: the sample buffer is live; its data is interleaved 32-bit float (the
            // audio settings asked for that).
            let Some(block) = (unsafe { sample.data_buffer() }) else { return };
            // SAFETY: the block buffer is live; copying its bytes out.
            let len = unsafe { block.data_length() };
            let mut bytes = vec![0u8; len];
            // SAFETY: `bytes` is `len` long.
            if unsafe { block.copy_data_bytes(0, len, std::ptr::NonNull::new(bytes.as_mut_ptr().cast()).unwrap_or(std::ptr::NonNull::dangling())) } != 0 {
                return;
            }
            let pcm: Vec<f32> = bytes.as_chunks::<4>().0.iter().map(|b| f32::from_ne_bytes(*b)).collect();
            if let Ok(mut s) = self.ivars().sound.lock() {
                s.push_back((pcm, ts));
                while s.len() > MAX_SOUND_PIECES {
                    s.pop_front();
                }
            }
            self.ivars().count.fetch_add(1, Ordering::Relaxed);
        }
    }

    unsafe impl AVCaptureAudioDataOutputSampleBufferDelegate for Output {}
);

impl Output {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this: Allocated<Self> = Self::alloc();
        let this = this.set_ivars(shared);
        // SAFETY: NSObject's init on a freshly allocated object with its ivars set.
        unsafe { msg_send![super(this), init] }
    }
}

define_class!(
    // SAFETY: NSObject may be subclassed; the ivars are an Arc dropped with the observer.
    #[unsafe(super(NSObject))]
    #[name = "FerrocastCaptureObserver"]
    #[ivars = Arc<Shared>]
    struct Observer;

    unsafe impl NSObjectProtocol for Observer {}

    impl Observer {
        #[unsafe(method(deviceDisconnected:))]
        fn device_disconnected(&self, note: &NSNotification) {
            if let Some(obj) = note.object()
                && let Ok(dev) = obj.downcast::<AVCaptureDevice>()
            {
                // SAFETY: reading uniqueID from a live AVCaptureDevice in notification.
                let id = unsafe { dev.uniqueID() }.to_string();
                handle_disconnect(self.ivars(), &id);
            }
        }

        #[unsafe(method(runtimeError:))]
        fn runtime_error(&self, note: &NSNotification) {
            let detail = runtime_error_detail(note);
            record_stopped(self.ivars(), format!("the {} stopped ({detail})", self.ivars().label));
        }

        #[unsafe(method(sessionInterrupted:))]
        fn session_interrupted(&self, _note: &NSNotification) {
            if !self.ivars().dropping.load(Ordering::Acquire) {
                self.ivars().interrupted.store(true, Ordering::Release);
            }
        }

        #[unsafe(method(interruptionEnded:))]
        fn interruption_ended(&self, _note: &NSNotification) {
            self.ivars().interrupted.store(false, Ordering::Release);
            if !self.ivars().dropping.load(Ordering::Acquire) {
                record_hotplug_connected(self.ivars().device_id.clone());
            }
        }

        #[unsafe(method(didStopRunning:))]
        fn did_stop_running(&self, _note: &NSNotification) {
            if !self.ivars().dropping.load(Ordering::Acquire) {
                record_stopped(self.ivars(), format!("the {} stopped running", self.ivars().label));
            }
        }
    }
);

impl Observer {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this: Allocated<Self> = Self::alloc();
        let this = this.set_ivars(shared);
        // SAFETY: NSObject's init on a freshly allocated object with its ivars set.
        unsafe { msg_send![super(this), init] }
    }
}

struct NotificationGuard(Retained<Observer>);

// SAFETY: `Observer`'s ivars are `Arc<Shared>` (atomics + mutexes), and removing from
// `NSNotificationCenter::defaultCenter` on Drop is thread-safe.
unsafe impl Send for NotificationGuard {}

impl NotificationGuard {
    fn attach(shared: Arc<Shared>, session: &AVCaptureSession) -> Self {
        let obs = Observer::new(shared);
        // SAFETY: `obs` stays alive inside `NotificationGuard` until `Drop`, which removes it
        // from `NSNotificationCenter::defaultCenter()` before deallocation.
        unsafe {
            let center = NSNotificationCenter::defaultCenter();
            center.addObserver_selector_name_object(
                &obs,
                sel!(deviceDisconnected:),
                Some(AVCaptureDeviceWasDisconnectedNotification),
                None,
            );
            center.addObserver_selector_name_object(
                &obs,
                sel!(runtimeError:),
                Some(AVCaptureSessionRuntimeErrorNotification),
                Some(session),
            );
            center.addObserver_selector_name_object(
                &obs,
                sel!(sessionInterrupted:),
                Some(AVCaptureSessionWasInterruptedNotification),
                Some(session),
            );
            center.addObserver_selector_name_object(
                &obs,
                sel!(interruptionEnded:),
                Some(AVCaptureSessionInterruptionEndedNotification),
                Some(session),
            );
            center.addObserver_selector_name_object(
                &obs,
                sel!(didStopRunning:),
                Some(AVCaptureSessionDidStopRunningNotification),
                Some(session),
            );
        }
        Self(obs)
    }
}

impl Drop for NotificationGuard {
    fn drop(&mut self) {
        // SAFETY: unregistering our live observer from `NSNotificationCenter::defaultCenter`.
        unsafe {
            NSNotificationCenter::defaultCenter().removeObserver(&self.0);
        }
    }
}

define_class!(
    // SAFETY: NSObject may be subclassed; the ivars are a Mutex dropped with the observer.
    #[unsafe(super(NSObject))]
    #[name = "FerrocastHotplugObserver"]
    #[ivars = Mutex<HashSet<String>>]
    struct HotplugObserver;

    unsafe impl NSObjectProtocol for HotplugObserver {}

    impl HotplugObserver {
        #[unsafe(method(hotplugConnected:))]
        fn hotplug_connected(&self, note: &NSNotification) {
            if let Some(obj) = note.object()
                && let Ok(dev) = obj.downcast::<AVCaptureDevice>()
            {
                // SAFETY: reading uniqueID from a live AVCaptureDevice in notification.
                let id = unsafe { dev.uniqueID() }.to_string();
                if let Ok(mut known) = self.ivars().lock() {
                    known.insert(id.clone());
                }
                record_hotplug_connected(id);
            }
        }

        #[unsafe(method(hotplugDisconnected:))]
        fn hotplug_disconnected(&self, note: &NSNotification) {
            if let Some(obj) = note.object()
                && let Ok(dev) = obj.downcast::<AVCaptureDevice>()
            {
                // SAFETY: reading uniqueID from a live AVCaptureDevice in notification.
                let id = unsafe { dev.uniqueID() }.to_string();
                if let Ok(mut known) = self.ivars().lock() {
                    known.remove(&id);
                }
                record_hotplug_disconnected(&id);
            }
        }

        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe_value(
            &self,
            _key_path: Option<&NSString>,
            _object: Option<&AnyObject>,
            _change: Option<&AnyObject>,
            _context: *mut c_void,
        ) {
            let next: HashSet<String> = discover_cameras().into_iter().map(|d| d.id).collect();
            let (added, removed) = {
                let Ok(mut known) = self.ivars().lock() else { return };
                let diff = diff_device_sets(&known, &next);
                *known = next;
                diff
            };
            for id in added {
                record_hotplug_connected(id);
            }
            for id in removed {
                record_hotplug_disconnected(&id);
            }
        }
    }
);

struct HotplugGuard {
    _observer: Retained<HotplugObserver>,
    _discovery: Vec<Retained<AVCaptureDeviceDiscoverySession>>,
}

// SAFETY: `HotplugGuard` lives forever in a `OnceLock` and only touches atomics/mutexes.
unsafe impl Send for HotplugGuard {}
// SAFETY: `HotplugGuard` is immutable after initialization in `OnceLock`.
unsafe impl Sync for HotplugGuard {}

fn ensure_hotplug_listener() {
    static ONCE: OnceLock<HotplugGuard> = OnceLock::new();
    ONCE.get_or_init(|| {
        let discovery = camera_discovery_sessions();
        let mut initial = HashSet::new();
        // SAFETY: reading `.devices()` from newly created discovery sessions before installing KVO.
        unsafe {
            for s in &discovery {
                for d in s.devices().iter() {
                    initial.insert(d.uniqueID().to_string());
                }
            }
        }
        let this: Allocated<HotplugObserver> = HotplugObserver::alloc();
        let this = this.set_ivars(Mutex::new(initial));
        // SAFETY: NSObject's init on freshly allocated observer, registered for process lifetime.
        let obs: Retained<HotplugObserver> = unsafe { msg_send![super(this), init] };
        let key_path = NSString::from_str("devices");
        let null_ctx: *mut c_void = std::ptr::null_mut();
        // SAFETY: observer and discovery sessions both stay alive for the process lifetime via
        // `OnceLock`, so notifications and KVO registrations never outlive either object.
        unsafe {
            let center = NSNotificationCenter::defaultCenter();
            center.addObserver_selector_name_object(
                &obs,
                sel!(hotplugConnected:),
                Some(AVCaptureDeviceWasConnectedNotification),
                None,
            );
            center.addObserver_selector_name_object(
                &obs,
                sel!(hotplugDisconnected:),
                Some(AVCaptureDeviceWasDisconnectedNotification),
                None,
            );
            for s in &discovery {
                let _: () =
                    msg_send![&**s, addObserver: &*obs, forKeyPath: &*key_path, options: 0usize, context: null_ctx];
            }
        }
        HotplugGuard { _observer: obs, _discovery: discovery }
    });
}

/// Returns newly connected device IDs if AVFoundation fired `WasConnected` since `since_seq`.
pub(crate) fn take_hotplug_connected(since_seq: &mut u64) -> Option<Vec<String>> {
    ensure_hotplug_listener();
    let cur = HOTPLUG_SEQ.load(Ordering::Acquire);
    if cur == *since_seq {
        return None;
    }
    let prev = *since_seq;
    *since_seq = cur;
    let ids = HOTPLUG_RECENT
        .lock()
        .map(|q| q.iter().filter(|(s, _)| *s > prev).map(|(_, id)| id.clone()).collect())
        .unwrap_or_default();
    Some(ids)
}

/// A running camera or microphone; dropping it stops.
pub struct Capture {
    session: Retained<AVCaptureSession>,
    device: Retained<AVCaptureDevice>,
    _observer: NotificationGuard,
    _output: Retained<Output>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
    shared: Arc<Shared>,
    /// The device opened for "the system's microphone": when the system's changes, this one
    /// has ended and the watchdog opens the new one (OBS's input follows the default too).
    follows: Option<String>,
}

impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Capture").field("received", &self.received()).finish()
    }
}

// SAFETY: AVCaptureSession may be started and stopped from any thread; the shared state is
// behind mutexes and atomics.
unsafe impl Send for Capture {}

fn open(id: &str, video: bool) -> Result<Retained<AVCaptureDeviceInput>, AvError> {
    // SAFETY: looking a device up by id and opening it; both return owned objects or errors.
    unsafe {
        let device = AVCaptureDevice::deviceWithUniqueID(&NSString::from_str(id)).ok_or_else(|| {
            AvError::Source(format!("the {} isn't connected", if video { "camera" } else { "microphone" }))
        })?;
        AVCaptureDeviceInput::deviceInputWithDevice_error(&device).map_err(|e| {
            AvError::Source(format!(
                "the {} couldn't open ({}); allow it for Ferrocast in System Settings → Privacy & Security, or close the app using it",
                if video { "camera" } else { "microphone" },
                e.localizedDescription()
            ))
        })
    }
}

/// The sizes and top frame rates a camera (by its unique id) has: what its Size and Frame rate
/// lists offer, so a capture card isn't offered a 4K it can't send. Empty when it isn't
/// connected.
pub fn modes(id: &str) -> Vec<(u32, u32, f64)> {
    // SAFETY: looking a device up by id and reading its formats; nothing is set.
    unsafe {
        let Some(device) = AVCaptureDevice::deviceWithUniqueID(&NSString::from_str(id)) else { return Vec::new() };
        device
            .formats()
            .iter()
            .map(|f| {
                let d = CMVideoFormatDescriptionGetDimensions(&f.formatDescription());
                let top = f.videoSupportedFrameRateRanges().iter().map(|r| r.maxFrameRate()).fold(0.0, f64::max);
                (u32::try_from(d.width).unwrap_or(0), u32::try_from(d.height).unwrap_or(0), top)
            })
            .filter(|m| m.0 > 0 && m.1 > 0)
            .collect()
    }
}

/// Each camera's own size (its format before Ferrocast first changed it), by unique id: what
/// "Its own" means for the rest of the run (AVFoundation keeps the last format set).
/// Each camera's own size, by unique id.
type OwnModes = std::collections::HashMap<String, (u32, u32)>;
static OWN: std::sync::Mutex<Option<OwnModes>> = std::sync::Mutex::new(None);

/// Active dimensions of `device`'s current format.
fn active_dims(device: &AVCaptureDevice) -> Option<(u32, u32)> {
    // SAFETY: reading active format dimensions of live device.
    let d = unsafe { CMVideoFormatDescriptionGetDimensions(&device.activeFormat().formatDescription()) };
    let dims = (u32::try_from(d.width).ok()?, u32::try_from(d.height).ok()?);
    (dims.0 > 0 && dims.1 > 0).then_some(dims)
}

/// Returns the device's pre-configuration size (if remembered) or its active format size,
/// without latching an arbitrary third-party format before Ferrocast configures the device.
fn own_of(device: &AVCaptureDevice) -> Option<(u32, u32)> {
    // SAFETY: reading uniqueID of live device.
    let id = unsafe { device.uniqueID() }.to_string();
    if let Ok(all) = OWN.lock()
        && let Some(cached) = all.as_ref().and_then(|m| m.get(&id).copied())
    {
        return Some(cached);
    }
    active_dims(device)
}

/// Remembers `device`'s initial format dims right before Ferrocast changes its `activeFormat`.
fn remember_own(device: &AVCaptureDevice) {
    let Some(dims) = active_dims(device) else { return };
    // SAFETY: reading uniqueID of live device.
    let id = unsafe { device.uniqueID() }.to_string();
    let mut all = OWN.lock().unwrap_or_else(|e| e.into_inner());
    all.get_or_insert_with(Default::default).entry(id).or_insert(dims);
}

/// A camera's own size (by unique id), as [`own_of`]; None when it isn't connected.
pub fn own_size(id: &str) -> Option<(u32, u32)> {
    // SAFETY: looking a device up by id.
    let device = unsafe { AVCaptureDevice::deviceWithUniqueID(&NSString::from_str(id)) }?;
    own_of(&device)
}

const FOURCC_MJPEG_DMB1: u32 = u32::from_be_bytes(*b"dmb1");
const FOURCC_JPEG: u32 = u32::from_be_bytes(*b"jpeg");
const USB_TRANSPORT: i32 = i32::from_be_bytes(*b"usb ");
/// Bluetooth inputs (AirPods, headsets): macOS drops them to a 16 kHz call profile when used.
const BLUETOOTH_TRANSPORT: i32 = i32::from_be_bytes(*b"blue");
const BLUETOOTH_LE_TRANSPORT: i32 = i32::from_be_bytes(*b"blea");

/// A camera format entry with its FourCC subtype.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Format {
    pub width: u32,
    pub height: u32,
    pub max_fps: f64,
    pub fourcc: u32,
}

fn fourcc_rank(f: &Format, usb: bool) -> u8 {
    let raw_bps = f64::from(f.width) * f64::from(f.height) * f.max_fps * 12.0;
    let prefer_mjpeg = usb && raw_bps > 300_000_000.0;
    if f.fourcc == FOURCC_MJPEG_DMB1 || f.fourcc == FOURCC_JPEG {
        return if prefer_mjpeg { 6 } else { 5 };
    }
    if f.fourcc == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        || f.fourcc == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
    {
        return if prefer_mjpeg { 5 } else { 6 };
    }
    if f.fourcc == kCVPixelFormatType_422YpCbCr8_yuvs
        || f.fourcc == kCVPixelFormatType_422YpCbCr8
        || f.fourcc == kCVPixelFormatType_422YpCbCr8FullRange
    {
        return 4;
    }
    if f.fourcc == kCVPixelFormatType_32BGRA
        || f.fourcc == kCVPixelFormatType_32ARGB
        || f.fourcc == kCVPixelFormatType_32RGBA
    {
        return 3;
    }
    if f.fourcc == kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange
        || f.fourcc == kCVPixelFormatType_420YpCbCr10BiPlanarFullRange
    {
        return 1;
    }
    2
}

fn compare_fps_and_fourcc(a: &Format, b: &Format, usb: bool) -> std::cmp::Ordering {
    // Treat nominal rate steps within 0.1 fps (e.g. 59.94 vs 60.0) as equivalent so USB2 MJPEG
    // beats uncompressed YUY2/422 even when the MJPEG entry is NTSC 59.94.
    if (a.max_fps - b.max_fps).abs() > 0.1 {
        return a.max_fps.total_cmp(&b.max_fps);
    }
    fourcc_rank(a, usb).cmp(&fourcc_rank(b, usb)).then_with(|| a.max_fps.total_cmp(&b.max_fps))
}

/// Format selector that also breaks ties by compressed vs raw FourCC on USB2 capture cards.
pub(crate) fn pick_format_for(
    formats: &[Format],
    size: Option<(u32, u32)>,
    fps: Option<u32>,
    own: Option<(u32, u32)>,
    usb: bool,
) -> Option<usize> {
    if size.is_none() && fps.is_none() {
        return None;
    }
    // Only a rate asked: its own size when that reaches it ("Its own" with 30 kept a card's
    // 1080p, not 1440p it only upscales to; review, 18 Nov).
    if size.is_none()
        && let Some((w, h)) = own
    {
        let want = f64::from(fps.unwrap_or(0));
        let best = formats
            .iter()
            .enumerate()
            .filter(|(_, f)| f.width == w && f.height == h && f.max_fps + 0.5 >= want)
            .max_by(|(_, a), (_, b)| compare_fps_and_fourcc(a, b, usb))
            .map(|(i, _)| i);
        if best.is_some() {
            return best;
        }
    }
    let want_fps = f64::from(fps.unwrap_or(0));
    let area = |(w, h): (u32, u32)| u64::from(w) * u64::from(h);
    // Within 2% of the shape asked.
    let (aw, ah) = size.unwrap_or((16, 9));
    let shaped = |f: &Format| {
        let (a, b) = (f64::from(f.width) * f64::from(ah), f64::from(f.height) * f64::from(aw));
        b > 0.0 && (a / b - 1.0).abs() <= 0.02
    };
    // Closer in size first (never over it when a size was asked), then reaching the frame rate,
    // then the higher rate / better FourCC.
    formats
        .iter()
        .enumerate()
        .filter(|(_, f)| size.is_none_or(|s| area((f.width, f.height)) <= area(s)))
        .max_by(|(_, a), (_, b)| {
            let size_of = |f: &Format| match size {
                Some(s) => area((f.width, f.height)) as i128 - area(s) as i128,
                None => area((f.width, f.height)) as i128,
            };
            let reaches = |f: &Format| f.max_fps + 0.5 >= want_fps;
            let (first, second) = match size {
                Some(_) => (size_of(a).cmp(&size_of(b)), reaches(a).cmp(&reaches(b))),
                None => (reaches(a).cmp(&reaches(b)), size_of(a).cmp(&size_of(b))),
            };
            shaped(a).cmp(&shaped(b)).then(first).then(second).then_with(|| compare_fps_and_fourcc(a, b, usb))
        })
        .map(|(i, _)| i)
}

/// The camera format to use for a size and frame rate asked: among `formats` (width, height,
/// highest frame rate), the asked size that reaches the frame rate; else the asked size at its
/// best rate; else the nearest size at or under it. None when nothing was asked, or no format.
pub fn pick_format(
    formats: &[(u32, u32, f64)],
    size: Option<(u32, u32)>,
    fps: Option<u32>,
    own: Option<(u32, u32)>,
) -> Option<usize> {
    let mapped: Vec<Format> =
        formats.iter().map(|&(width, height, max_fps)| Format { width, height, max_fps, fourcc: 0 }).collect();
    pick_format_for(&mapped, size, fps, own, false)
}

/// When a USB capture card defaults to a slow uncompressed format (<24 fps, e.g. MacroSilicon
/// 1080p5 `yuvs`) under "Its own" settings, synthesize a rate-only spec so `configure` picks the
/// full-rate MJPEG mode instead of leaving it stuck at 5 fps.
fn auto_usb_spec(id: &str, own: Option<(u32, u32)>, active_top_fps: f64, formats: &[Format]) -> Option<CameraSpec> {
    if !(active_top_fps > 0.0 && active_top_fps < 24.0) {
        return None;
    }
    if !formats.iter().any(|f| f.max_fps >= 24.0) {
        return None;
    }
    let own_max = own
        .map(|(w, h)| formats.iter().filter(|f| f.width == w && f.height == h).map(|f| f.max_fps).fold(0.0, f64::max))
        .unwrap_or(0.0);
    let target_fps = if own_max >= 59.5 {
        60
    } else if own_max >= 24.0 {
        own_max.round() as u32
    } else {
        30
    };
    Some(CameraSpec { id: id.to_string(), size: None, fps: Some(target_fps) })
}

fn device_formats(
    device: &AVCaptureDevice,
) -> (Retained<NSArray<objc2_av_foundation::AVCaptureDeviceFormat>>, Vec<Format>) {
    // SAFETY: reading formats and immutable format descriptions of live device.
    unsafe {
        let formats = device.formats();
        let list = formats
            .iter()
            .map(|f| {
                let desc = f.formatDescription();
                let d = CMVideoFormatDescriptionGetDimensions(&desc);
                let fourcc = desc.media_sub_type();
                let max_fps = f.videoSupportedFrameRateRanges().iter().map(|r| r.maxFrameRate()).fold(0.0, f64::max);
                Format {
                    width: u32::try_from(d.width).unwrap_or(0),
                    height: u32::try_from(d.height).unwrap_or(0),
                    max_fps,
                    fourcc,
                }
            })
            .collect();
        (formats, list)
    }
}

fn auto_usb_spec_for_device(device: &AVCaptureDevice, id: &str) -> Option<CameraSpec> {
    // SAFETY: reading transportType and activeFormat of live device.
    let (usb, active_top_fps) = unsafe {
        let usb = device.transportType() == USB_TRANSPORT;
        let top =
            device.activeFormat().videoSupportedFrameRateRanges().iter().map(|r| r.maxFrameRate()).fold(0.0, f64::max);
        (usb, top)
    };
    if !usb {
        return None;
    }
    let (_, list) = device_formats(device);
    auto_usb_spec(id, own_of(device), active_top_fps, &list)
}

fn clamp_frame_duration(want_fps: u32, min_dur: CMTime, max_dur: CMTime) -> CMTime {
    // SAFETY: constructing and comparing valid CoreMedia CMTime values.
    unsafe {
        let want = CMTime::new(1, want_fps.clamp(1, 1000) as i32);
        if want.compare(min_dur) < 0 {
            min_dur
        } else if want.compare(max_dur) > 0 {
            max_dur
        } else {
            want
        }
    }
}

struct DeviceLock<'a>(&'a AVCaptureDevice);

impl<'a> DeviceLock<'a> {
    fn acquire(device: &'a AVCaptureDevice) -> Option<Self> {
        // SAFETY: locking live device for configuration; paired with `unlockForConfiguration` on Drop.
        unsafe { device.lockForConfiguration() }.ok().map(|()| Self(device))
    }
}

impl Drop for DeviceLock<'_> {
    fn drop(&mut self) {
        // SAFETY: unlocked 1-to-1 with `DeviceLock::acquire`.
        unsafe { self.0.unlockForConfiguration() };
    }
}

/// Applies the chosen format & clamped frame duration to a device already locked with `DeviceLock`.
fn apply_format(device: &AVCaptureDevice, spec: &CameraSpec) {
    let (formats, list) = device_formats(device);
    let own = own_of(device);
    // SAFETY: reading transportType of live device.
    let usb = unsafe { device.transportType() == USB_TRANSPORT };
    let Some(i) = pick_format_for(&list, spec.size, spec.fps, own, usb) else { return };
    let Some(format) = formats.iter().nth(i) else { return };
    tracing::info!(
        asked = ?(spec.size, spec.fps),
        picked = ?list[i],
        usb,
        modes = list.len(),
        "the camera's mode for the size and frame rate asked"
    );
    let set = std::panic::AssertUnwindSafe(|| {
        // SAFETY: the format belongs to `device` and every duration is clamped within the format's
        // hardware min/max frame durations via `CMTimeCompare`.
        unsafe {
            device.setActiveFormat(&format);
            if let Some(want_fps) = spec.fps {
                let want = f64::from(want_fps);
                let ranges = format.videoSupportedFrameRateRanges();
                let inside = ranges.iter().find(|r| r.minFrameRate() - 0.5 <= want && want <= r.maxFrameRate() + 0.5);
                let time = match inside {
                    Some(r) if (r.maxFrameRate() - r.minFrameRate()).abs() < 0.01 => Some(r.minFrameDuration()),
                    Some(r) => Some(clamp_frame_duration(want_fps, r.minFrameDuration(), r.maxFrameDuration())),
                    None => {
                        let gap = |r: &objc2_av_foundation::AVFrameRateRange| {
                            if want > r.maxFrameRate() { want - r.maxFrameRate() } else { r.minFrameRate() - want }
                        };
                        ranges
                            .iter()
                            .min_by(|a, b| gap(a).total_cmp(&gap(b)))
                            .map(|r| if want > r.maxFrameRate() { r.minFrameDuration() } else { r.maxFrameDuration() })
                    }
                };
                if let Some(t) = time {
                    device.setActiveVideoMinFrameDuration(t);
                    device.setActiveVideoMaxFrameDuration(t);
                }
            }
        }
    });
    if let Err(e) = objc2::exception::catch(set) {
        tracing::warn!(
            "the camera wouldn't take that size or frame rate; it keeps its own: {}",
            e.map(|e| e.to_string()).unwrap_or_default()
        );
    }
}

fn pick_output_pixel_format(available: &[u32]) -> u32 {
    const PREFERRED: [u32; 7] = [
        kCVPixelFormatType_32BGRA,
        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
        kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
        kCVPixelFormatType_422YpCbCr8,
        kCVPixelFormatType_422YpCbCr8_yuvs,
        kCVPixelFormatType_32ARGB,
        kCVPixelFormatType_32RGBA,
    ];
    PREFERRED.into_iter().find(|f| available.contains(f)).unwrap_or(kCVPixelFormatType_32BGRA)
}

impl Capture {
    /// What this capture wants said on the dashboard, once (how a default microphone was chosen).
    pub fn take_notice(&self) -> Option<crate::notices::Notice> {
        self.shared.pending_notice.lock().ok().and_then(|mut n| n.take())
    }

    /// Starts a camera: its frames as BGRA (or supported YUV converted cleanly), at the size and
    /// frame rate asked (or full-rate on slow-default USB capture cards when left "Its own").
    pub fn camera(spec: &CameraSpec) -> Result<Self, AvError> {
        ensure_hotplug_listener();
        let input = open(&spec.id, true)?;
        // SAFETY: reading live device from newly created AVCaptureDeviceInput.
        let device = unsafe { input.device() };
        let shared = Arc::new(Shared { device_id: spec.id.clone(), label: "camera", ..Shared::default() });
        let delegate = Output::new(shared.clone());
        let queue = DispatchQueue::new("ferrocast.camera", DispatchQueueAttr::SERIAL);
        // SAFETY: building and starting a capture session with live objects, on this thread.
        unsafe {
            let session = AVCaptureSession::new();
            let observer = NotificationGuard::attach(shared.clone(), &session);
            let output = AVCaptureVideoDataOutput::new();
            let avail: Vec<u32> =
                output.availableVideoCVPixelFormatTypes().iter().map(|n| n.unsignedIntValue()).collect();
            let pixfmt = pick_output_pixel_format(&avail);
            let key: &NSString = &*(std::ptr::from_ref(kCVPixelBufferPixelFormatTypeKey).cast::<NSString>());
            let val = NSNumber::new_u32(pixfmt);
            let settings: Retained<NSDictionary<NSString, AnyObject>> =
                NSDictionary::from_slices(&[key], &[val.as_ref()]);
            output.setVideoSettings(Some(&settings));
            output.setAlwaysDiscardsLateVideoFrames(true);
            output.setSampleBufferDelegate_queue(Some(ProtocolObject::from_ref(&*delegate)), Some(&queue));
            session.beginConfiguration();
            if !session.canAddInput(&input) || !session.canAddOutput(&output) {
                session.commitConfiguration();
                return Err(AvError::Source("the camera can't be used this way".into()));
            }
            session.addInput(&input);
            session.addOutput(&output);
            let effective_spec = if spec.size.is_none() && spec.fps.is_none() {
                auto_usb_spec_for_device(&device, &spec.id)
            } else {
                Some(spec.clone())
            };
            let _dev_lock = effective_spec.as_ref().and_then(|cfg_spec| {
                if session.canSetSessionPreset(AVCaptureSessionPresetInputPriority) {
                    session.setSessionPreset(AVCaptureSessionPresetInputPriority);
                }
                remember_own(&device);
                let lock = DeviceLock::acquire(&device)?;
                apply_format(&device, cfg_spec);
                Some(lock)
            });
            session.commitConfiguration();
            drop(_dev_lock);
            session.startRunning();
            Ok(Self { session, device, _observer: observer, _output: delegate, _queue: queue, shared, follows: None })
        }
    }

    /// Starts a microphone: its sound as interleaved stereo float at 48 kHz.
    pub fn microphone(id: &str) -> Result<Self, AvError> {
        ensure_hotplug_listener();
        let (follows, notice) = if is_default(id) {
            let default = default_microphone().ok_or_else(|| AvError::Source("there's no microphone".into()))?;
            let inputs: Vec<(String, String)> = microphones().into_iter().map(|d| (d.id, d.name)).collect();
            let last = LAST_PLAIN_MIC.lock().unwrap_or_else(|p| p.into_inner()).clone();
            let pick = pick_default_mic(&default, &inputs, last.as_deref(), is_bluetooth_input);
            let notice = pick.swapped_from.as_ref().map(|name| crate::notices::Notice {
                key: "mic-bluetooth",
                line: format!("Using {}: your AirPods mic would drop the sound to phone quality.", pick.name),
                detail: format!(
                    "The system's microphone is {name}, a Bluetooth headset: macOS would switch it to a \
                     16 kHz call mode. Pick it by name in the source's settings to use it anyway."
                ),
            });
            if !is_bluetooth_input(&pick.id) {
                *LAST_PLAIN_MIC.lock().unwrap_or_else(|p| p.into_inner()) = Some(pick.id.clone());
            }
            (Some(pick.id), notice)
        } else {
            (None, None)
        };
        let input = open(follows.as_deref().unwrap_or(id), false)?;
        // SAFETY: reading live device from newly created AVCaptureDeviceInput.
        let device = unsafe { input.device() };
        // SAFETY: reading uniqueID of live device.
        let device_id = unsafe { device.uniqueID() }.to_string();
        let shared = Arc::new(Shared {
            device_id,
            label: "microphone",
            pending_notice: Mutex::new(notice),
            ..Shared::default()
        });
        let delegate = Output::new(shared.clone());
        let queue = DispatchQueue::new("ferrocast.microphone", DispatchQueueAttr::SERIAL);
        // SAFETY: as for the camera.
        unsafe {
            let session = AVCaptureSession::new();
            let observer = NotificationGuard::attach(shared.clone(), &session);
            let output = AVCaptureAudioDataOutput::new();
            let keys = [
                "AVFormatIDKey",
                "AVSampleRateKey",
                "AVNumberOfChannelsKey",
                "AVLinearPCMIsFloatKey",
                "AVLinearPCMBitDepthKey",
                "AVLinearPCMIsNonInterleaved",
            ]
            .map(NSString::from_str);
            let values = [
                NSNumber::new_u32(u32::from_be_bytes(*b"lpcm")),
                NSNumber::new_f64(48_000.0),
                NSNumber::new_u32(2),
                NSNumber::new_bool(true),
                NSNumber::new_u32(32),
                NSNumber::new_bool(false),
            ];
            let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
            let value_refs: Vec<&AnyObject> = values.iter().map(|v| -> &AnyObject { v.as_ref() }).collect();
            let settings: Retained<NSDictionary<NSString, AnyObject>> =
                NSDictionary::from_slices(&key_refs, &value_refs);
            output.setAudioSettings(Some(&settings));
            output.setSampleBufferDelegate_queue(Some(ProtocolObject::from_ref(&*delegate)), Some(&queue));
            session.beginConfiguration();
            if !session.canAddInput(&input) || !session.canAddOutput(&output) {
                session.commitConfiguration();
                return Err(AvError::Source("the microphone can't be used this way".into()));
            }
            session.addInput(&input);
            session.addOutput(&output);
            session.commitConfiguration();
            session.startRunning();
            Ok(Self { session, device, _observer: observer, _output: delegate, _queue: queue, shared, follows })
        }
    }

    /// The newest camera frame and its host time (taken). With no copy (the default), frames
    /// come from [`Capture::take_surface`] instead, and this gives a copy of one when asked.
    pub fn take_frame(&self) -> Option<(Picture, u64)> {
        if let Some(f) = self.shared.frame.lock().ok().and_then(|mut f| f.take()) {
            return Some(f);
        }
        let (s, ts) = self.shared.surface.lock().ok().and_then(|mut f| f.take())?;
        Some((s.copy()?, ts))
    }

    /// The newest camera frame as the camera holds it, for the card to read with no copy (taken).
    pub fn take_surface(&self) -> Option<super::screen::Surface> {
        self.shared.surface.lock().ok().and_then(|mut f| f.take()).map(|(s, _)| s)
    }

    /// Every sound piece since the last call, oldest first, each with its host time.
    pub fn take_sound(&self) -> Vec<(Vec<f32>, u64)> {
        self.shared.sound.lock().map(|mut s| s.drain(..).collect()).unwrap_or_default()
    }

    /// Frames or sound pieces received since the start.
    pub fn received(&self) -> u64 {
        self.shared.count.load(Ordering::Relaxed)
    }

    /// A camera or microphone sends all the time: quiet for long means it stopped.
    pub fn steady(&self) -> bool {
        true
    }

    /// True while an AVFoundation session is temporarily interrupted on a still-connected device.
    pub fn interrupted(&self) -> bool {
        if let Ok(stopped) = self.shared.stopped.lock()
            && stopped.is_some()
        {
            return false;
        }
        // SAFETY: checking whether the capture device is still connected and session is interrupted.
        unsafe {
            self.device.isConnected()
                && (self.shared.interrupted.load(Ordering::Acquire) || self.session.isInterrupted())
        }
    }

    /// Why it ended, if it did: reported immediately on disconnect, permanent stop running, or
    /// session runtime error instead of waiting for `STALL_NS`. Temporary interruptions on a
    /// still-connected device return `None` so [`Self::interrupted`] keeps the session alive for
    /// `AVCaptureSessionInterruptionEndedNotification`.
    pub fn ended(&self) -> Option<String> {
        let stopped = self.shared.stopped.lock().ok().and_then(|s| s.clone());
        // SAFETY: reading interruption, running, and device connection flags from live AVFoundation handles.
        let (interrupted, running, connected) = unsafe {
            (
                self.shared.interrupted.load(Ordering::Acquire) || self.session.isInterrupted(),
                self.session.isRunning(),
                self.device.isConnected(),
            )
        };
        if let Some(why) = session_ended_reason(stopped.as_deref(), interrupted, running, connected, self.shared.label)
        {
            return Some(why);
        }
        // Following the system's microphone, and it changed (headphones with a mic plugged
        // in, another input picked in System Settings): this one is done, the new one opens.
        let now = self.follows.as_ref().and_then(|_| default_microphone());
        let moved = moved_on(self.follows.as_deref(), now.as_deref(), is_bluetooth_input);
        // Said once, not at every look (the watchdog asks every second).
        if moved.is_none()
            && let (Some(was), Some(now)) = (self.follows.as_deref(), now.as_deref())
            && was != now
            && !self.shared.bluetooth_noted.swap(true, Ordering::Relaxed)
        {
            tracing::info!("the system's microphone is now a Bluetooth one; staying on the one in use");
        }
        moved
    }
}

fn session_ended_reason(
    stopped: Option<&str>,
    interrupted: bool,
    running: bool,
    connected: bool,
    label: &'static str,
) -> Option<String> {
    if let Some(reason) = stopped {
        return Some(reason.to_string());
    }
    if !connected {
        return Some("the capture device was disconnected".into());
    }
    if !interrupted && !running {
        return Some(format!("the {label} stopped running"));
    }
    None
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.shared.dropping.store(true, Ordering::Release);
        // SAFETY: the session is live; stopping it ends the callbacks.
        unsafe { self.session.stopRunning() };
    }
}

/// Why a capture following the system's microphone ended: the system's is another one now.
/// (None when there's no system microphone: the one open stays until one comes.)
///
/// Not when it moved to a Bluetooth input from one that isn't: putting on AirPods makes them the
/// system's microphone, and opening it drops them to the 16 kHz call profile, which ruins the
/// stream's voice and the streamer's own headphone sound. The mic open stays; picking the
/// Bluetooth one by name still works (GAPS #7).
fn moved_on(follows: Option<&str>, now: Option<&str>, bluetooth: impl Fn(&str) -> bool) -> Option<String> {
    match (follows, now) {
        (Some(was), Some(now)) if was != now => {
            if bluetooth(now) && !bluetooth(was) {
                return None;
            }
            Some("the system's microphone changed".into())
        }
        _ => None,
    }
}

/// The last microphone a "default" source opened that wasn't a Bluetooth one.
static LAST_PLAIN_MIC: Mutex<Option<String>> = Mutex::new(None);

/// What a "default" microphone opens.
#[derive(Debug, PartialEq, Eq)]
struct DefaultPick {
    id: String,
    name: String,
    /// The Bluetooth headset (its name) the system's microphone is, that this stays off.
    swapped_from: Option<String>,
}

/// The system's microphone, unless it is a Bluetooth hands-free one and any other input exists:
/// then the last plain one this source used, else the first plain one (macOS would drop a
/// Bluetooth headset to a 16 kHz call mode). A Bluetooth mic picked by name never comes here.
fn pick_default_mic(
    default: &str,
    inputs: &[(String, String)],
    last: Option<&str>,
    bluetooth: impl Fn(&str) -> bool,
) -> DefaultPick {
    let name_of = |id: &str| inputs.iter().find(|(i, _)| i == id).map(|(_, n)| n.clone()).unwrap_or_else(|| id.into());
    if !bluetooth(default) {
        return DefaultPick { id: default.into(), name: name_of(default), swapped_from: None };
    }
    let plain = last
        .filter(|l| inputs.iter().any(|(i, _)| i == l) && !bluetooth(l))
        .map(str::to_string)
        .or_else(|| inputs.iter().find(|(i, _)| !bluetooth(i)).map(|(i, _)| i.clone()));
    match plain {
        Some(id) => DefaultPick { name: name_of(&id), id, swapped_from: Some(name_of(default)) },
        None => DefaultPick { id: default.into(), name: name_of(default), swapped_from: None },
    }
}

/// Whether the input with this unique id is a Bluetooth one.
fn is_bluetooth_input(id: &str) -> bool {
    // SAFETY: looking a device up by id and reading its transport type.
    unsafe {
        AVCaptureDevice::deviceWithUniqueID(&NSString::from_str(id))
            .is_some_and(|d| matches!(d.transportType(), BLUETOOTH_TRANSPORT | BLUETOOTH_LE_TRANSPORT))
    }
}

#[cfg(test)]
mod tests {
    use super::{is_default, moved_on, pick_format};

    #[test]
    fn the_systems_microphone_is_followed() {
        assert!(is_default("") && is_default("default") && !is_default("BuiltInMicrophoneDevice"));
        let none = |_: &str| false;
        assert_eq!(moved_on(None, Some("b"), none), None, "a picked mic stays");
        assert_eq!(moved_on(Some("a"), Some("a"), none), None);
        assert_eq!(moved_on(Some("a"), None, none), None, "no system mic now: keep what's open");
        assert!(moved_on(Some("a"), Some("b"), none).is_some(), "the system's changed: open the new one");
    }

    #[test]
    fn a_default_mic_that_is_bluetooth_opens_another_input_when_there_is_one() {
        use super::{DefaultPick, pick_default_mic};
        let inputs: Vec<(String, String)> = [("usb", "KREO KAST"), ("builtin", "MacBook Mic"), ("pods", "AirPods")]
            .into_iter()
            .map(|(i, n)| (i.to_string(), n.to_string()))
            .collect();
        let bt = |id: &str| id == "pods";
        // A plain default is used as is.
        assert_eq!(
            pick_default_mic("builtin", &inputs, None, bt),
            DefaultPick { id: "builtin".into(), name: "MacBook Mic".into(), swapped_from: None }
        );
        // AirPods as the default: the first plain input...
        let first = pick_default_mic("pods", &inputs, None, bt);
        assert_eq!((first.id.as_str(), first.swapped_from.as_deref()), ("usb", Some("AirPods")));
        // ...or the last plain one this source used.
        let last = pick_default_mic("pods", &inputs, Some("builtin"), bt);
        assert_eq!(last.id, "builtin");
        // A remembered mic that's gone, or Bluetooth, isn't used.
        assert_eq!(pick_default_mic("pods", &inputs, Some("gone"), bt).id, "usb");
        assert_eq!(pick_default_mic("pods", &inputs, Some("pods"), bt).id, "usb");
        // Only Bluetooth inputs: it opens it (there's nothing else).
        let only = vec![("pods".to_string(), "AirPods".to_string())];
        let p = pick_default_mic("pods", &only, None, bt);
        assert_eq!((p.id.as_str(), p.swapped_from), ("pods", None));
    }

    #[test]
    fn airpods_becoming_the_systems_mic_dont_take_the_stream_over() {
        let bt = |id: &str| id == "airpods";
        assert_eq!(moved_on(Some("usb-mic"), Some("airpods"), bt), None, "stays on the mic in use");
        assert!(moved_on(Some("airpods"), Some("usb-mic"), bt).is_some(), "leaving Bluetooth follows");
        assert!(moved_on(Some("airpods"), Some("airpods-2"), |id: &str| id.starts_with("airpods")).is_some());
        assert!(moved_on(Some("usb-mic"), Some("builtin"), bt).is_some(), "other moves follow as before");
    }

    // A webcam's formats: (width, height, highest frame rate).
    const CAM: [(u32, u32, f64); 5] =
        [(640, 480, 30.0), (1280, 720, 30.0), (1280, 720, 60.0), (1920, 1080, 30.0), (3840, 2160, 15.0)];

    #[test]
    fn nothing_asked_keeps_the_cameras_own() {
        assert_eq!(pick_format(&CAM, None, None, None), None);
    }

    #[test]
    fn the_asked_size_at_the_asked_rate() {
        assert_eq!(pick_format(&CAM, Some((1280, 720)), Some(60), None), Some(2));
        assert_eq!(pick_format(&CAM, Some((1920, 1080)), Some(30), None), Some(3));
    }

    #[test]
    fn a_rate_it_cant_reach_still_gets_the_size() {
        // 1080p only goes to 30: the size wins, at its best rate.
        assert_eq!(pick_format(&CAM, Some((1920, 1080)), Some(60), None), Some(3));
    }

    #[test]
    fn a_size_it_doesnt_have_gets_the_nearest_under_it() {
        assert_eq!(pick_format(&CAM, Some((1600, 900)), None, None), Some(2));
        assert_eq!(pick_format(&CAM, Some((320, 240)), None, None), None);
    }

    #[test]
    fn only_a_rate_picks_the_biggest_that_reaches_it() {
        assert_eq!(pick_format(&CAM, None, Some(60), None), Some(2));
        // 30 is reached by 1080p, the biggest that does (4K only goes to 15).
        assert_eq!(pick_format(&CAM, None, Some(30), None), Some(3));
    }

    // Odd modes like a USB capture card's (the owner's, 17 Nov: 4K picked 1920×2160, squeezed,
    // and 60 alone 1280×1024, cropped). Made-up lists around those; the real card has no
    // 1080p60 and now gets 2560×1440 at 30 and 1360×768 at 60.
    const CARD: [(u32, u32, f64); 6] = [
        (1920, 2160, 30.0),
        (1920, 1080, 30.0),
        (1280, 1024, 60.0),
        (1280, 720, 60.0),
        (1920, 1080, 60.0),
        (640, 480, 60.0),
    ];

    #[test]
    fn the_shape_asked_comes_before_a_bigger_odd_mode() {
        assert_eq!(pick_format(&CARD, Some((3840, 2160)), Some(30), None), Some(4));
        assert_eq!(pick_format(&CARD, None, Some(60), None), Some(4));
        let usb2 = &CARD[..4];
        assert_eq!(pick_format(usb2, Some((3840, 2160)), Some(30), None), Some(1));
        assert_eq!(pick_format(usb2, None, Some(60), None), Some(3));
        // No mode of that shape: the old rules still pick one.
        assert_eq!(pick_format(&[(1280, 1024, 60.0)], None, Some(60), None), Some(0));
    }

    /// "Its own" means the size the camera had: a rate it reaches there keeps it, one it
    /// doesn't moves to the biggest 16:9 that does.
    #[test]
    fn its_own_size_is_kept_when_it_reaches_the_rate() {
        let own = Some((1920, 1080));
        assert_eq!(pick_format(&CARD, None, Some(30), own), Some(4), "1080p's best mode reaching 30");
        let usb2 = &CARD[..4];
        assert_eq!(pick_format(usb2, None, Some(30), own), Some(1));
        assert_eq!(pick_format(usb2, None, Some(60), own), Some(3), "1080p doesn't reach 60 here");
        assert_eq!(pick_format(usb2, None, None, own), None, "nothing asked: the session's preset keeps its own");
        assert_eq!(pick_format(usb2, Some((1280, 720)), Some(30), own), Some(3), "a size asked wins");
    }

    #[test]
    fn ntsc_5994_reaches_60_fps_ask() {
        let ntsc_card = [(1920, 1080, 59.94), (1280, 720, 60.0)];
        assert_eq!(pick_format(&ntsc_card, None, Some(60), None), Some(0));
    }

    #[test]
    fn pick_format_prefers_usb2_mjpeg_and_fourcc_over_yuvs() {
        use super::{FOURCC_MJPEG_DMB1, Format, auto_usb_spec, pick_format_for};
        use objc2_core_video::{
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
            kCVPixelFormatType_422YpCbCr8_yuvs,
        };
        let dmb1 = FOURCC_MJPEG_DMB1;
        let yuvs = kCVPixelFormatType_422YpCbCr8_yuvs;
        let nv12 = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
        let p010 = kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange;

        // Cheap MacroSilicon USB2 capture card listing uncompressed yuvs and compressed MJPEG at 1080p.
        let usb2_modes = [
            Format { width: 1920, height: 1080, max_fps: 60.0, fourcc: yuvs },
            Format { width: 1920, height: 1080, max_fps: 59.94, fourcc: dmb1 },
            Format { width: 1920, height: 1080, max_fps: 30.0, fourcc: p010 },
        ];
        assert_eq!(
            pick_format_for(&usb2_modes, Some((1920, 1080)), Some(60), None, true),
            Some(1),
            "USB2 HD card must prefer MJPEG (dmb1) over uncompressed yuvs"
        );

        // Continuity Camera / non-USB prefers native 8-bit 420v over compressed MJPEG / 10-bit.
        let phone_modes = [
            Format { width: 1920, height: 1080, max_fps: 30.0, fourcc: dmb1 },
            Format { width: 1920, height: 1080, max_fps: 30.0, fourcc: nv12 },
            Format { width: 1920, height: 1080, max_fps: 30.0, fourcc: p010 },
        ];
        assert_eq!(pick_format_for(&phone_modes, Some((1920, 1080)), Some(30), None, false), Some(1));

        // When USB card defaults to 1080p@5 yuvs under "Its own", auto_usb_spec upgrades to full rate.
        let throttled_card = [
            Format { width: 1920, height: 1080, max_fps: 5.0, fourcc: yuvs },
            Format { width: 1920, height: 1080, max_fps: 60.0, fourcc: dmb1 },
            Format { width: 1280, height: 720, max_fps: 60.0, fourcc: dmb1 },
        ];
        let spec = auto_usb_spec("usb-card", Some((1920, 1080)), 5.0, &throttled_card)
            .unwrap_or_else(|| panic!("throttled card should upgrade"));
        assert_eq!(spec.fps, Some(60));
        assert_eq!(pick_format_for(&throttled_card, spec.size, spec.fps, Some((1920, 1080)), true), Some(1));

        // Card whose 1080p caps at 5 fps falls back to 720p60 rather than staying a slideshow.
        let slow_1080 = [
            Format { width: 1920, height: 1080, max_fps: 5.0, fourcc: yuvs },
            Format { width: 1280, height: 720, max_fps: 60.0, fourcc: dmb1 },
        ];
        let spec = auto_usb_spec("usb-card", Some((1920, 1080)), 5.0, &slow_1080)
            .unwrap_or_else(|| panic!("should step down to full rate"));
        assert_eq!(pick_format_for(&slow_1080, spec.size, spec.fps, Some((1920, 1080)), true), Some(1));

        // Healthy default already >= 24 fps stays untouched.
        assert!(auto_usb_spec("webcam", Some((1920, 1080)), 30.0, &throttled_card).is_none());
    }

    #[test]
    fn clamp_frame_duration_keeps_ntsc_within_range() {
        use super::clamp_frame_duration;
        use objc2_core_media::CMTime;
        // Hardware range [5.0 .. 59.94] -> min duration 1001/60000, max duration 1/5.
        // SAFETY: constructing and comparing valid CoreMedia CMTimes for unit tests.
        unsafe {
            let min_dur = CMTime::new(1001, 60000);
            let max_dur = CMTime::new(1, 5);
            let clamped_60 = clamp_frame_duration(60, min_dur, max_dur);
            assert_eq!(clamped_60.compare(min_dur), 0, "60 fps must clamp to 1001/60000");

            let clamped_slow = clamp_frame_duration(2, min_dur, max_dur);
            assert_eq!(clamped_slow.compare(max_dur), 0, "2 fps must clamp to 1/5");

            let inside_30 = clamp_frame_duration(30, min_dur, max_dur);
            let want_30 = CMTime::new(1, 30);
            assert_eq!(inside_30.compare(want_30), 0);
        }
    }

    #[test]
    fn pick_output_pixel_format_prefers_bgra_with_yuv_fallback() {
        use super::pick_output_pixel_format;
        use objc2_core_video::{
            kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVPixelFormatType_422YpCbCr8,
        };
        assert_eq!(
            pick_output_pixel_format(&[kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVPixelFormatType_32BGRA]),
            kCVPixelFormatType_32BGRA
        );
        assert_eq!(
            pick_output_pixel_format(&[kCVPixelFormatType_422YpCbCr8, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange]),
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        );
        assert_eq!(pick_output_pixel_format(&[kCVPixelFormatType_422YpCbCr8]), kCVPixelFormatType_422YpCbCr8);
    }

    /// GAPS #10 said 10-bit (HDR) frames from a card would be dropped by `copy_video_frame`.
    /// They never reach it: the output is told to deliver an 8-bit format, so AVFoundation
    /// converts, and the choice never lands on a 10-bit one even when that is all the card lists.
    #[test]
    fn the_output_is_never_asked_for_a_ten_bit_format() {
        use super::pick_output_pixel_format;
        use objc2_core_video::{kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange};
        let x420 = u32::from_be_bytes(*b"x420");
        let l10r = u32::from_be_bytes(*b"l10r");
        let p010 = u32::from_be_bytes(*b"p010");
        assert_eq!(
            pick_output_pixel_format(&[x420, l10r, p010, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange]),
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        );
        assert_eq!(pick_output_pixel_format(&[x420, l10r, p010]), kCVPixelFormatType_32BGRA);
    }

    #[test]
    fn disconnect_and_runtime_error_end_capture_immediately() {
        use super::{OWN, Observer, Shared, handle_disconnect};
        use objc2::rc::Retained;
        use objc2::runtime::AnyObject;
        use objc2::{AnyThread, msg_send};
        use objc2_av_foundation::AVCaptureSessionErrorKey;
        use objc2_foundation::{NSDictionary, NSError, NSNotification, NSString};
        use std::sync::Arc;

        OWN.lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(Default::default)
            .insert("cam-target".into(), (1920, 1080));

        let shared = Arc::new(Shared { device_id: "cam-target".into(), label: "camera", ..Shared::default() });
        handle_disconnect(&shared, "cam-other");
        assert!(shared.stopped.lock().ok().and_then(|s| s.clone()).is_none(), "must ignore other device");

        handle_disconnect(&shared, "cam-target");
        assert!(
            shared.stopped.lock().ok().and_then(|s| s.clone()).is_some_and(|m| m.contains("disconnected")),
            "matching disconnect must end capture immediately"
        );
        assert!(
            !OWN.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|m| m.contains_key("cam-target")),
            "disconnect must clear cached OWN size"
        );

        let shared2 = Arc::new(Shared { device_id: "cam-2".into(), label: "camera", ..Shared::default() });
        let obs = Observer::new(shared2.clone());
        let domain = NSString::from_str("AVFoundationErrorDomain");
        // SAFETY: building synthetic NSError + NSDictionary + NSNotification for runtimeError: dispatch test.
        let err = unsafe { NSError::errorWithDomain_code_userInfo(&domain, -11815, None) };
        // SAFETY: `AVCaptureSessionErrorKey` is a static immutable Foundation NSString constant.
        let key: &NSString = unsafe { AVCaptureSessionErrorKey };
        let typed_info: Retained<NSDictionary<NSString, AnyObject>> =
            NSDictionary::from_slices(&[key], &[err.as_ref()]);
        // SAFETY: `NSDictionary<K, V>` and `NSDictionary` have identical Objective-C runtime representation.
        let user_info: &NSDictionary = unsafe { &*(Retained::as_ptr(&typed_info).cast()) };
        let note_name = NSString::from_str("AVCaptureSessionRuntimeErrorNotification");
        // SAFETY: initializing synthetic NSNotification with live strings and dictionary.
        let note = unsafe {
            NSNotification::initWithName_object_userInfo(NSNotification::alloc(), &note_name, None, Some(user_info))
        };
        // SAFETY: invoking ObjC `runtimeError:` handler on live `Observer`.
        let _: () = unsafe { msg_send![&*obs, runtimeError: &*note] };
        assert!(
            shared2.stopped.lock().ok().and_then(|s| s.clone()).is_some(),
            "runtimeError notification must end capture immediately"
        );
    }

    fn hotplug_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn hotplug_tracker_records_connected_devices() {
        use super::{HOTPLUG_SEQ, record_hotplug_connected, take_hotplug_connected};
        use std::sync::atomic::Ordering;

        let _guard = hotplug_test_lock();
        let mut seq = HOTPLUG_SEQ.load(Ordering::Acquire);
        assert_eq!(take_hotplug_connected(&mut seq), None);
        record_hotplug_connected("cam-usb-replugged".into());
        let connected = take_hotplug_connected(&mut seq).unwrap_or_default();
        assert!(connected.iter().any(|id| id == "cam-usb-replugged"));
        assert_eq!(take_hotplug_connected(&mut seq), None);
    }

    #[test]
    fn session_interruption_and_stop_running_end_capture() {
        use super::{Observer, Shared};
        use objc2::{AnyThread, msg_send, sel};
        use objc2_foundation::{NSNotification, NSObjectProtocol, NSString};
        use std::sync::Arc;

        let _guard = hotplug_test_lock();
        let shared = Arc::new(Shared { device_id: "cam-iphone".into(), label: "camera", ..Shared::default() });
        let obs = Observer::new(shared.clone());
        assert!(obs.respondsToSelector(sel!(sessionInterrupted:)), "must observe WasInterrupted");
        assert!(obs.respondsToSelector(sel!(interruptionEnded:)), "must observe InterruptionEnded");
        assert!(obs.respondsToSelector(sel!(didStopRunning:)), "must observe DidStopRunning");

        let note_int = NSString::from_str("AVCaptureSessionWasInterruptedNotification");
        // SAFETY: creating synthetic NSNotification for sessionInterrupted: handler.
        let int_note =
            unsafe { NSNotification::initWithName_object_userInfo(NSNotification::alloc(), &note_int, None, None) };
        // SAFETY: invoking sessionInterrupted: on live `Observer`.
        let _: () = unsafe { msg_send![&*obs, sessionInterrupted: &*int_note] };
        assert!(shared.interrupted.load(std::sync::atomic::Ordering::Acquire), "WasInterrupted must flag interrupted");

        let seq_before = super::HOTPLUG_SEQ.load(std::sync::atomic::Ordering::Acquire);
        let note_end = NSString::from_str("AVCaptureSessionInterruptionEndedNotification");
        // SAFETY: creating synthetic NSNotification for interruptionEnded: handler.
        let end_note =
            unsafe { NSNotification::initWithName_object_userInfo(NSNotification::alloc(), &note_end, None, None) };
        // SAFETY: invoking interruptionEnded: on live `Observer`.
        let _: () = unsafe { msg_send![&*obs, interruptionEnded: &*end_note] };
        assert!(
            !shared.interrupted.load(std::sync::atomic::Ordering::Acquire),
            "InterruptionEnded must clear interrupted"
        );
        assert!(super::HOTPLUG_SEQ.load(std::sync::atomic::Ordering::Acquire) > seq_before);

        let name = NSString::from_str("AVCaptureSessionDidStopRunningNotification");
        // SAFETY: creating synthetic NSNotification for didStopRunning: handler.
        let note = unsafe { NSNotification::initWithName_object_userInfo(NSNotification::alloc(), &name, None, None) };
        // SAFETY: invoking didStopRunning: on live `Observer`.
        let _: () = unsafe { msg_send![&*obs, didStopRunning: &*note] };
        assert!(
            shared.stopped.lock().ok().and_then(|s| s.clone()).is_some(),
            "DidStopRunning must immediately mark capture ended"
        );
    }

    #[test]
    fn dropping_observer_suppresses_interruption_ended_hotplug_bump() {
        use super::{Observer, Shared};
        use objc2::{AnyThread, msg_send};
        use objc2_foundation::{NSNotification, NSString};
        use std::sync::Arc;
        use std::sync::atomic::Ordering;

        let _guard = hotplug_test_lock();
        let shared = Arc::new(Shared { device_id: "cam-iphone".into(), label: "camera", ..Shared::default() });
        shared.dropping.store(true, Ordering::Release);
        let obs = Observer::new(shared.clone());

        let seq_before = super::HOTPLUG_SEQ.load(Ordering::Acquire);
        let epoch_before = super::HOTPLUG_EPOCH.load(Ordering::Acquire);
        let note_end = NSString::from_str("AVCaptureSessionInterruptionEndedNotification");
        // SAFETY: creating synthetic NSNotification for interruptionEnded: handler.
        let end_note =
            unsafe { NSNotification::initWithName_object_userInfo(NSNotification::alloc(), &note_end, None, None) };
        // SAFETY: invoking interruptionEnded: on live `Observer` while dropping is set.
        let _: () = unsafe { msg_send![&*obs, interruptionEnded: &*end_note] };
        assert_eq!(super::HOTPLUG_SEQ.load(Ordering::Acquire), seq_before);
        assert_eq!(super::HOTPLUG_EPOCH.load(Ordering::Acquire), epoch_before);
    }

    #[test]
    fn interrupted_connected_session_keeps_running_while_disconnect_or_stop_ends_it() {
        use super::session_ended_reason;

        // Temporary interruption with device still connected stays alive for interruptionEnded:.
        assert_eq!(session_ended_reason(None, true, true, true, "camera"), None);
        assert_eq!(session_ended_reason(None, true, false, true, "camera"), None);
        // Physical disconnect or permanent stop/error still ends immediately even if interrupted.
        assert!(session_ended_reason(None, true, false, false, "camera").is_some());
        assert!(session_ended_reason(Some("the camera stopped running"), true, false, true, "camera").is_some());
        assert!(session_ended_reason(None, false, false, true, "camera").is_some());
    }

    #[test]
    fn hotplug_epoch_bumps_on_disconnect_and_discovery_diff() {
        use super::{diff_device_sets, hotplug_epoch, record_hotplug_connected, record_hotplug_disconnected};
        use std::collections::HashSet;

        let _guard = hotplug_test_lock();
        let e0 = hotplug_epoch();
        record_hotplug_connected("cam-continuity".into());
        let e1 = hotplug_epoch();
        assert!(e1 > e0, "connect must bump hotplug_epoch");

        record_hotplug_disconnected("cam-continuity");
        let e2 = hotplug_epoch();
        assert!(e2 > e1, "disconnect must bump hotplug_epoch so device lists refresh");

        let prev: HashSet<String> = ["builtin".into(), "iphone".into()].into_iter().collect();
        let next: HashSet<String> = ["builtin".into(), "deskview".into()].into_iter().collect();
        let (added, removed) = diff_device_sets(&prev, &next);
        assert_eq!(added, vec!["deskview"]);
        assert_eq!(removed, vec!["iphone"]);
    }
}
