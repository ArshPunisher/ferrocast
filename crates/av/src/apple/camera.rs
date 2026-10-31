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
    AVCaptureDeviceInput, AVCaptureOutput, AVCaptureSession, AVCaptureVideoDataOutput,
    AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeAudio, AVMediaTypeVideo,
};
use objc2_core_media::CMSampleBuffer;
use objc2_foundation::{NSDictionary, NSNumber, NSString};

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
#[derive(Debug, Default)]
struct Shared {
    frame: Mutex<Option<(Picture, u64)>>,
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

impl Capture {
    /// Starts a camera: its frames as BGRA at the camera's own size.
    pub fn camera(id: &str) -> Result<Self, AvError> {
        let input = open(id, true)?;
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

    /// The newest camera frame and its host time (taken).
    pub fn take_frame(&self) -> Option<(Picture, u64)> {
        self.shared.frame.lock().ok().and_then(|mut f| f.take())
    }

    /// Every sound piece since the last call, oldest first, each with its host time.
    pub fn take_sound(&self) -> Vec<(Vec<f32>, u64)> {
        self.shared.sound.lock().map(|mut s| s.drain(..).collect()).unwrap_or_default()
    }

    /// Frames or sound pieces received since the start.
    pub fn received(&self) -> u64 {
        self.shared.count.load(Ordering::Relaxed)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // SAFETY: the session is live; stopping it ends the callbacks.
        unsafe { self.session.stopRunning() };
    }
}
