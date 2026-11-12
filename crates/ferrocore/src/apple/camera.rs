//! Cameras and microphones with AVFoundation: a device's frames (BGRA, the newest kept) or its
//! sound (stereo float at 48 kHz, every piece kept in order with its time). Times are the Mac's
//! host clock in nanoseconds, the same as screen capture's, so everything lines up.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{
    AVCaptureAudioDataOutput, AVCaptureAudioDataOutputSampleBufferDelegate, AVCaptureConnection, AVCaptureDevice,
    AVCaptureDeviceInput, AVCaptureOutput, AVCaptureSession, AVCaptureSessionPresetInputPriority,
    AVCaptureVideoDataOutput, AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeAudio, AVMediaTypeVideo,
};
use objc2_core_media::{CMSampleBuffer, CMTime, CMVideoFormatDescriptionGetDimensions};
use objc2_foundation::{NSDictionary, NSNumber, NSString};

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

fn list(video: bool) -> Vec<Device> {
    // SAFETY: AVFoundation's constant media types; listing devices has no other requirement.
    unsafe {
        let Some(kind) = (if video { AVMediaTypeVideo } else { AVMediaTypeAudio }) else { return Vec::new() };
        #[allow(deprecated)]
        AVCaptureDevice::devicesWithMediaType(kind)
            .iter()
            .map(|d| Device { id: d.uniqueID().to_string(), name: d.localizedName().to_string() })
            .collect()
    }
}

/// Cameras connected now.
pub fn cameras() -> Vec<Device> {
    list(true)
}

/// Microphones and other sound inputs connected now.
pub fn microphones() -> Vec<Device> {
    list(false)
}

/// What a capture's delegate keeps.
#[derive(Default)]
struct Shared {
    frame: Mutex<Option<(Picture, u64)>>,
    /// The newest frame as the camera holds it (no copy), when the card can read it directly.
    surface: Mutex<Option<(super::screen::Surface, u64)>>,
    sound: Mutex<VecDeque<(Vec<f32>, u64)>>,
    count: AtomicU64,
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
                // frames are); copied out only when that's switched off.
                // A frame that can't be kept that way (not BGRA) is copied below (CodeRabbit).
                if super::zerocopy::enabled()
                    && let Some(s) = super::screen::Surface::of(image.clone())
                {
                    if let Ok(mut f) = self.ivars().surface.lock() {
                        *f = Some((s, ts));
                    }
                    self.ivars().count.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                if let Some(p) = super::screen::copy_bgra(&image) {
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

/// A running camera or microphone; dropping it stops.
pub struct Capture {
    session: Retained<AVCaptureSession>,
    _output: Retained<Output>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
    shared: Arc<Shared>,
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

/// The camera format to use for a size and frame rate asked: among `formats` (width, height,
/// highest frame rate), the asked size that reaches the frame rate; else the asked size at its
/// best rate; else the nearest size at or under it. None when nothing was asked, or no format.
pub fn pick_format(formats: &[(u32, u32, f64)], size: Option<(u32, u32)>, fps: Option<u32>) -> Option<usize> {
    if size.is_none() && fps.is_none() {
        return None;
    }
    let want_fps = f64::from(fps.unwrap_or(0));
    let area = |(w, h): (u32, u32)| u64::from(w) * u64::from(h);
    // Closer in size first (never over it when a size was asked), then reaching the frame rate,
    // then the higher rate.
    formats
        .iter()
        .enumerate()
        .filter(|(_, f)| size.is_none_or(|s| area((f.0, f.1)) <= area(s)))
        .max_by(|(_, a), (_, b)| {
            let size_of = |f: &(u32, u32, f64)| match size {
                Some(s) => area((f.0, f.1)) as i128 - area(s) as i128,
                None => area((f.0, f.1)) as i128,
            };
            let reaches = |f: &(u32, u32, f64)| f.2 + 0.01 >= want_fps;
            // A size asked comes first; a frame rate alone puts reaching it first.
            let (first, second) = match size {
                Some(_) => (size_of(a).cmp(&size_of(b)), reaches(a).cmp(&reaches(b))),
                None => (reaches(a).cmp(&reaches(b)), size_of(a).cmp(&size_of(b))),
            };
            first.then(second).then(a.2.total_cmp(&b.2))
        })
        .map(|(i, _)| i)
}

/// Asks the camera for the size and frame rate in `spec` (the nearest it has); a camera that
/// can't be configured keeps its own. AVFoundation throws (an Objective-C exception, which
/// would end the app) for a value it doesn't take: frame times come from the format's own
/// ranges, and anything thrown is caught and logged (owner's test, 10 Nov: 720p at 30 ended the
/// app).
fn configure(device: &AVCaptureDevice, spec: &CameraSpec) {
    // SAFETY: reading the device's formats and their ranges; nothing is set here.
    let (formats, list) = unsafe {
        let formats = device.formats();
        let list: Vec<(u32, u32, f64)> = formats
            .iter()
            .map(|f| {
                let d = CMVideoFormatDescriptionGetDimensions(&f.formatDescription());
                let top = f.videoSupportedFrameRateRanges().iter().map(|r| r.maxFrameRate()).fold(0.0, f64::max);
                (u32::try_from(d.width).unwrap_or(0), u32::try_from(d.height).unwrap_or(0), top)
            })
            .collect();
        (formats, list)
    };
    let Some(i) = pick_format(&list, spec.size, spec.fps) else { return };
    let Some(format) = formats.iter().nth(i) else { return };
    tracing::info!(
        asked = ?(spec.size, spec.fps),
        picked = ?list[i],
        modes = list.len(),
        "the camera's mode for the size and frame rate asked"
    );
    // SAFETY: locked for configuration, as AVFoundation asks; unlocked whatever happens.
    if unsafe { device.lockForConfiguration() }.is_err() {
        return;
    }
    let set = std::panic::AssertUnwindSafe(|| {
        // SAFETY: the format is one the device lists, and each frame time is one of the
        // format's own (an exact range end) or inside one of its ranges.
        unsafe {
            device.setActiveFormat(&format);
            if let Some(want) = spec.fps.map(f64::from) {
                let ranges = format.videoSupportedFrameRateRanges();
                let inside = ranges.iter().find(|r| r.minFrameRate() - 0.01 <= want && want <= r.maxFrameRate() + 0.01);
                let time = match inside {
                    // A range of one rate: its own time, exactly as the camera writes it.
                    Some(r) if (r.maxFrameRate() - r.minFrameRate()).abs() < 0.01 => Some(r.minFrameDuration()),
                    Some(_) => Some(CMTime::new(1, want.round() as i32)),
                    // Outside every range (faster, slower, or in a gap): the nearest rate it has,
                    // that range's own end.
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
    // SAFETY: locked above.
    unsafe { device.unlockForConfiguration() };
}

impl Capture {
    /// Starts a camera: its frames as BGRA, at the size and frame rate asked (its own when
    /// none).
    pub fn camera(spec: &CameraSpec) -> Result<Self, AvError> {
        let input = open(&spec.id, true)?;
        let shared = Arc::new(Shared::default());
        let delegate = Output::new(shared.clone());
        let queue = DispatchQueue::new("ferrocast.camera", DispatchQueueAttr::SERIAL);
        // SAFETY: building and starting a capture session with live objects, on this thread.
        unsafe {
            let session = AVCaptureSession::new();
            let output = AVCaptureVideoDataOutput::new();
            let key = NSString::from_str("PixelFormatType");
            let bgra = NSNumber::new_u32(u32::from_be_bytes(*b"BGRA"));
            let settings: Retained<NSDictionary<NSString, AnyObject>> =
                NSDictionary::from_slices(&[&*key], &[bgra.as_ref()]);
            output.setVideoSettings(Some(&settings));
            output.setAlwaysDiscardsLateVideoFrames(true);
            output.setSampleBufferDelegate_queue(Some(ProtocolObject::from_ref(&*delegate)), Some(&queue));
            if !session.canAddInput(&input) || !session.canAddOutput(&output) {
                return Err(AvError::Source("the camera can't be used this way".into()));
            }
            session.addInput(&input);
            session.addOutput(&output);
            session.startRunning();
            // Once it runs (set before, the session's preset put the camera's own mode back:
            // owner's test, 10 Nov), with the session told the camera's mode leads.
            if spec.size.is_some() || spec.fps.is_some() {
                if session.canSetSessionPreset(AVCaptureSessionPresetInputPriority) {
                    session.setSessionPreset(AVCaptureSessionPresetInputPriority);
                }
                configure(&input.device(), spec);
            }
            Ok(Self { session, _output: delegate, _queue: queue, shared })
        }
    }

    /// Starts a microphone: its sound as interleaved stereo float at 48 kHz.
    pub fn microphone(id: &str) -> Result<Self, AvError> {
        let input = open(id, false)?;
        let shared = Arc::new(Shared::default());
        let delegate = Output::new(shared.clone());
        let queue = DispatchQueue::new("ferrocast.microphone", DispatchQueueAttr::SERIAL);
        // SAFETY: as for the camera.
        unsafe {
            let session = AVCaptureSession::new();
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
            if !session.canAddInput(&input) || !session.canAddOutput(&output) {
                return Err(AvError::Source("the microphone can't be used this way".into()));
            }
            session.addInput(&input);
            session.addOutput(&output);
            session.startRunning();
            Ok(Self { session, _output: delegate, _queue: queue, shared })
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

    /// Why it ended, if it did (AVFoundation's session keeps running; a stall shows instead).
    pub fn ended(&self) -> Option<String> {
        None
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // SAFETY: the session is live; stopping it ends the callbacks.
        unsafe { self.session.stopRunning() };
    }
}

#[cfg(test)]
mod tests {
    use super::pick_format;

    // A webcam's formats: (width, height, highest frame rate).
    const CAM: [(u32, u32, f64); 5] =
        [(640, 480, 30.0), (1280, 720, 30.0), (1280, 720, 60.0), (1920, 1080, 30.0), (3840, 2160, 15.0)];

    #[test]
    fn nothing_asked_keeps_the_cameras_own() {
        assert_eq!(pick_format(&CAM, None, None), None);
    }

    #[test]
    fn the_asked_size_at_the_asked_rate() {
        assert_eq!(pick_format(&CAM, Some((1280, 720)), Some(60)), Some(2));
        assert_eq!(pick_format(&CAM, Some((1920, 1080)), Some(30)), Some(3));
    }

    #[test]
    fn a_rate_it_cant_reach_still_gets_the_size() {
        // 1080p only goes to 30: the size wins, at its best rate.
        assert_eq!(pick_format(&CAM, Some((1920, 1080)), Some(60)), Some(3));
    }

    #[test]
    fn a_size_it_doesnt_have_gets_the_nearest_under_it() {
        assert_eq!(pick_format(&CAM, Some((1600, 900)), None), Some(2));
        assert_eq!(pick_format(&CAM, Some((320, 240)), None), None);
    }

    #[test]
    fn only_a_rate_picks_the_biggest_that_reaches_it() {
        assert_eq!(pick_format(&CAM, None, Some(60)), Some(2));
        // 30 is reached by 1080p, the biggest that does (4K only goes to 15).
        assert_eq!(pick_format(&CAM, None, Some(30)), Some(3));
    }
}
