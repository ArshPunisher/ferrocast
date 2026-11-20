//! H.264 and HEVC on the Mac's media engine (VideoToolbox): real-time, no frame reordering (B
//! frames) so packets leave in order with no delay, keyframes every N seconds, Rec. 709 colours.

use std::ffi::c_void;
use std::ptr;
use std::sync::{Arc, Mutex};

use super::{
    CFArrayGetCount, CFArrayGetValueAtIndex, CFDictionaryGetValue, CFDictionaryRef, CFStringRef, CFTypeRef, Dict,
    OSStatus, Owned, array, data_bytes, float, int, kCFBooleanFalse, kCFBooleanTrue, string,
};
use crate::AvError;
use crate::video::{Frame, PixelFormat, RateControl, VideoCodec, VideoConfig, VideoPacket};

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

/// kCMTimeFlags_Valid.
const VALID: u32 = 1;
/// Times go to and from VideoToolbox in microseconds.
const US: i32 = 1_000_000;

impl CMTime {
    fn us(v: i64) -> Self {
        Self { value: v, timescale: US, flags: VALID, epoch: 0 }
    }

    fn invalid() -> Self {
        Self { value: 0, timescale: 0, flags: 0, epoch: 0 }
    }

    /// In microseconds; None when not set.
    fn to_us(self) -> Option<i64> {
        if self.flags & VALID == 0 || self.timescale <= 0 {
            return None;
        }
        Some((i128::from(self.value) * i128::from(US) / i128::from(self.timescale)) as i64)
    }
}

type Callback = extern "C" fn(*mut c_void, *mut c_void, OSStatus, u32, *const c_void);

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    static kVTCompressionPropertyKey_RealTime: CFStringRef;
    static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
    static kVTCompressionPropertyKey_ConstantBitRate: CFStringRef;
    static kVTCompressionPropertyKey_DataRateLimits: CFStringRef;
    static kVTCompressionPropertyKey_Quality: CFStringRef;
    static kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration: CFStringRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
    static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
    static kVTCompressionPropertyKey_ColorPrimaries: CFStringRef;
    static kVTCompressionPropertyKey_TransferFunction: CFStringRef;
    static kVTCompressionPropertyKey_YCbCrMatrix: CFStringRef;
    static kVTProfileLevel_H264_High_AutoLevel: CFStringRef;
    static kVTProfileLevel_HEVC_Main_AutoLevel: CFStringRef;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;
    fn VTCompressionSessionCreate(
        alloc: CFTypeRef,
        width: i32,
        height: i32,
        codec: u32,
        spec: CFDictionaryRef,
        source_attrs: CFDictionaryRef,
        data_alloc: CFTypeRef,
        callback: Callback,
        refcon: *mut c_void,
        out: *mut CFTypeRef,
    ) -> OSStatus;
    fn VTSessionSetProperty(session: CFTypeRef, key: CFStringRef, value: CFTypeRef) -> OSStatus;
    fn VTCompressionSessionPrepareToEncodeFrames(session: CFTypeRef) -> OSStatus;
    fn VTCompressionSessionGetPixelBufferPool(session: CFTypeRef) -> CFTypeRef;
    fn VTCompressionSessionEncodeFrame(
        session: CFTypeRef,
        image: CFTypeRef,
        pts: CMTime,
        duration: CMTime,
        props: CFDictionaryRef,
        frame_refcon: *mut c_void,
        flags_out: *mut u32,
    ) -> OSStatus;
    fn VTCompressionSessionCompleteFrames(session: CFTypeRef, until: CMTime) -> OSStatus;
    fn VTCompressionSessionInvalidate(session: CFTypeRef);
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    static kCVPixelBufferWidthKey: CFStringRef;
    static kCVPixelBufferHeightKey: CFStringRef;
    static kCVPixelBufferIOSurfacePropertiesKey: CFStringRef;
    static kCVImageBufferColorPrimaries_ITU_R_709_2: CFStringRef;
    static kCVImageBufferTransferFunction_ITU_R_709_2: CFStringRef;
    static kCVImageBufferYCbCrMatrix_ITU_R_709_2: CFStringRef;
    static kCVImageBufferColorPrimariesKey: CFStringRef;
    static kCVImageBufferTransferFunctionKey: CFStringRef;
    static kCVImageBufferYCbCrMatrixKey: CFStringRef;
    fn CVPixelBufferPoolCreatePixelBuffer(alloc: CFTypeRef, pool: CFTypeRef, out: *mut CFTypeRef) -> i32;
    fn CVPixelBufferLockBaseAddress(buf: CFTypeRef, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buf: CFTypeRef, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddress(buf: CFTypeRef) -> *mut u8;
    fn CVPixelBufferGetBytesPerRow(buf: CFTypeRef) -> usize;
    fn CVPixelBufferGetBaseAddressOfPlane(buf: CFTypeRef, plane: usize) -> *mut u8;
    fn CVPixelBufferGetBytesPerRowOfPlane(buf: CFTypeRef, plane: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(buf: CFTypeRef, plane: usize) -> usize;
    fn CVPixelBufferGetHeight(buf: CFTypeRef) -> usize;
    fn CVBufferSetAttachment(buf: CFTypeRef, key: CFStringRef, value: CFTypeRef, mode: u32);
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    static kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms: CFStringRef;
    static kCMSampleAttachmentKey_NotSync: CFStringRef;
    fn CMSampleBufferGetDataBuffer(sbuf: *const c_void) -> CFTypeRef;
    fn CMSampleBufferGetPresentationTimeStamp(sbuf: *const c_void) -> CMTime;
    fn CMSampleBufferGetDecodeTimeStamp(sbuf: *const c_void) -> CMTime;
    fn CMSampleBufferGetFormatDescription(sbuf: *const c_void) -> CFTypeRef;
    fn CMSampleBufferGetSampleAttachmentsArray(sbuf: *const c_void, create: u8) -> CFTypeRef;
    fn CMFormatDescriptionGetExtension(desc: CFTypeRef, key: CFStringRef) -> CFTypeRef;
    fn CMBlockBufferGetDataLength(buf: CFTypeRef) -> usize;
    fn CMBlockBufferCopyDataBytes(buf: CFTypeRef, offset: usize, len: usize, dest: *mut c_void) -> OSStatus;
}

/// FourCCs: 'avc1', 'hvc1', 'BGRA', '420v'.
const AVC1: u32 = u32::from_be_bytes(*b"avc1");
const HVC1: u32 = u32::from_be_bytes(*b"hvc1");
const BGRA: i32 = i32::from_be_bytes(*b"BGRA");
const NV12: i32 = i32::from_be_bytes(*b"420v");
/// kVTEncodeInfo_FrameDropped.
const DROPPED: u32 = 1 << 1;
/// kCVAttachmentMode_ShouldPropagate.
const PROPAGATE: u32 = 1;

/// What the encoder's callback fills, on VideoToolbox's own thread.
#[derive(Debug, Default)]
struct Sink {
    packets: Vec<VideoPacket>,
    header: Option<Vec<u8>>,
    error: Option<String>,
    /// The status behind `error`, to tell a lost session from a bad frame.
    status: Option<OSStatus>,
}

/// One VideoToolbox session.
#[derive(Debug)]
pub(crate) struct Encoder {
    session: Owned,
    /// The callback's refcon points here; it lives as long as the session (invalidated first).
    sink: Arc<Mutex<Sink>>,
    input: PixelFormat,
    height: usize,
    frame_us: i64,
    /// To make the session again when the system takes it away.
    cfg: VideoConfig,
    /// The bitrate now (it may have been changed live): what a new session starts with.
    rate_now: (u32, RateControl),
    /// The last failure was a lost session (see `recoverable`), not a bad frame.
    lost: bool,
    /// After a rebuild: the lost session's codec header, until the new one's is known and
    /// compared with it (`header_changed`).
    prior_header: Option<Vec<u8>>,
    /// When sessions were rebuilt lately (at most [`REBUILDS`] in [`REBUILD_WINDOW`], then the
    /// output gives up: a machine whose encoder keeps dying would else rebuild every frame).
    rebuilt_at: std::collections::VecDeque<std::time::Instant>,
}

/// Rebuilds of a lost session allowed in the window.
const REBUILDS: usize = 3;
const REBUILD_WINDOW: std::time::Duration = std::time::Duration::from_secs(30);

// SAFETY: the session is only used through `&mut self` (one thread at a time); VideoToolbox
// sessions may be used from any thread, and the sink is behind a mutex.
unsafe impl Send for Encoder {}

/// The bitrate, as OBS's own VideoToolbox encoder sets it; also on a running session (a new
/// bitrate while live, as OBS's dynamic bitrate does).
fn set_rate(session: &Owned, kbps: u32, rate: RateControl) -> Result<(), AvError> {
    let none = || AvError::Encoder("out of memory setting the video bitrate".into());
    let set = |key: CFStringRef, value: CFTypeRef| -> OSStatus {
        // SAFETY: the session is live; key is a VideoToolbox constant, value a live CF object.
        unsafe { VTSessionSetProperty(session.0, key, value) }
    };
    let bits = int(i32::try_from(u64::from(kbps) * 1000).unwrap_or(i32::MAX)).ok_or_else(none)?;
    // SAFETY: VideoToolbox's constant keys.
    let (cbr, avg, limits, quality) = unsafe {
        (
            kVTCompressionPropertyKey_ConstantBitRate,
            kVTCompressionPropertyKey_AverageBitRate,
            kVTCompressionPropertyKey_DataRateLimits,
            kVTCompressionPropertyKey_Quality,
        )
    };
    let limit = || -> OSStatus {
        // At most a second's worth of bits in any second.
        let (Some(bytes), Some(secs)) = (float(f64::from(kbps) * 125.0), float(1.0)) else { return -1 };
        array(&[&bytes, &secs]).map_or(-1, |a| set(limits, a.0))
    };
    match rate {
        RateControl::Cbr => {
            // Apple Silicon (macOS 13+) ConstantBitRate pads still frames, though M4-04 measured
            // transitions peaking above target (~7.3–9.0 Mbps over 2 s on a 6 Mbps target).
            // Elsewhere (or when CappedCbr asks for a strict cap) use AverageBitRate + 1 s
            // DataRateLimits.
            if set(cbr, bits.0) != 0 {
                let s = set(avg, bits.0);
                if s != 0 {
                    return Err(fail("set the bitrate", s));
                }
                limit();
            }
        }
        RateControl::CappedCbr => {
            // Setting DataRateLimits alongside ConstantBitRate disables filler padding anyway on
            // Apple Silicon, so strict platform caps use AverageBitRate + a 1 s byte ceiling
            // (measured <= 5,930 kbps 2 s peak on 6,000 kbps target across hard noise cuts).
            let s = set(avg, bits.0);
            if s != 0 {
                return Err(fail("set the bitrate", s));
            }
            limit();
        }
        // x264's rate factor never reaches here (VideoEncoder refuses it); an average if it did.
        RateControl::Average | RateControl::Crf(_) => {
            let s = set(avg, bits.0);
            if s != 0 {
                return Err(fail("set the bitrate", s));
            }
        }
        RateControl::Quality(q) => {
            let q = float(f64::from(q.clamp(0.0, 1.0))).ok_or_else(none)?;
            if set(quality, q.0) != 0 {
                let s = set(avg, bits.0);
                if s != 0 {
                    return Err(fail("set the bitrate", s));
                }
            }
        }
    }
    Ok(())
}

/// Statuses that mean the session is gone, not that the frame was bad: the system took it back
/// (display sleep, screen lock, the media server restarting) and Apple's way is to make a new one.
/// -12903 kVTInvalidSessionErr, -12911 kVTVideoEncoderMalfunctionErr,
/// -12915 kVTVideoEncoderNotAvailableNowErr.
fn recoverable(status: OSStatus) -> bool {
    matches!(status, -12903 | -12911 | -12915)
}

fn fail(what: &str, status: OSStatus) -> AvError {
    AvError::Encoder(format!("the Mac's video encoder refused to {what} (error {status})"))
}

impl Encoder {
    pub(crate) fn new(cfg: &VideoConfig) -> Result<Self, AvError> {
        let none = || AvError::Encoder("out of memory setting up the video encoder".into());
        let attrs = Dict::new().ok_or_else(none)?;
        let fmt = int(if cfg.input == PixelFormat::Bgra { BGRA } else { NV12 }).ok_or_else(none)?;
        let w = int(cfg.width as i32).ok_or_else(none)?;
        let h = int(cfg.height as i32).ok_or_else(none)?;
        let surface = Dict::new().ok_or_else(none)?;
        // SAFETY: the keys are CoreVideo's constant CFStrings, valid for the process's life.
        unsafe {
            attrs.set(kCVPixelBufferPixelFormatTypeKey, fmt.0);
            attrs.set(kCVPixelBufferWidthKey, w.0);
            attrs.set(kCVPixelBufferHeightKey, h.0);
            attrs.set(kCVPixelBufferIOSurfacePropertiesKey, surface.0.0);
        }
        let sink = Arc::new(Mutex::new(Sink::default()));
        let mut session: CFTypeRef = ptr::null();
        let codec = if cfg.codec == VideoCodec::H264 { AVC1 } else { HVC1 };
        // SAFETY: every pointer is valid for the call; the refcon is the sink, which outlives the
        // session (Drop invalidates the session before the Arc can go). The session is +1.
        let status = unsafe {
            VTCompressionSessionCreate(
                ptr::null(),
                cfg.width as i32,
                cfg.height as i32,
                codec,
                ptr::null(),
                attrs.0.0,
                ptr::null(),
                output,
                Arc::as_ptr(&sink).cast_mut().cast(),
                &raw mut session,
            )
        };
        if status != 0 {
            return Err(fail("start", status));
        }
        let session = Owned::new(session).ok_or_else(|| fail("start", -1))?;
        let fps = f64::from(cfg.fps_num) / f64::from(cfg.fps_den);
        let set = |key: CFStringRef, value: CFTypeRef| -> OSStatus {
            // SAFETY: the session is live; key is a VideoToolbox constant, value a live CF object.
            unsafe { VTSessionSetProperty(session.0, key, value) }
        };
        let gop = float(f64::from(cfg.keyframe_secs.max(1))).ok_or_else(none)?;
        let rate = float(fps).ok_or_else(none)?;
        // SAFETY: reading VideoToolbox's and CoreVideo's constant CFStrings and CF booleans.
        let required = unsafe {
            [
                (kVTCompressionPropertyKey_RealTime, kCFBooleanTrue, "run in real time"),
                (kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse, "turn off B-frames"),
                (kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, gop.0, "set the keyframe interval"),
                (
                    kVTCompressionPropertyKey_ProfileLevel,
                    if cfg.codec == VideoCodec::H264 {
                        kVTProfileLevel_H264_High_AutoLevel
                    } else {
                        kVTProfileLevel_HEVC_Main_AutoLevel
                    },
                    "set the profile",
                ),
            ]
        };
        for (key, value, what) in required {
            let s = set(key, value);
            if s != 0 {
                return Err(fail(what, s));
            }
        }
        set_rate(&session, cfg.bitrate_kbps, cfg.rate)?;
        // Hints and colour tags: the encoder may ignore them.
        let key_frames = int(i32::try_from(cfg.keyframe_secs.max(1) * fps.round().max(1.0) as u32).unwrap_or(i32::MAX));
        // SAFETY: as above, constants only.
        unsafe {
            if let (Some(k), Some(kf)) = (string(c"MaxKeyFrameInterval"), key_frames) {
                set(k.0.cast(), kf.0);
            }
            set(kVTCompressionPropertyKey_ExpectedFrameRate, rate.0);
            set(kVTCompressionPropertyKey_ColorPrimaries, kCVImageBufferColorPrimaries_ITU_R_709_2);
            set(kVTCompressionPropertyKey_TransferFunction, kCVImageBufferTransferFunction_ITU_R_709_2);
            set(kVTCompressionPropertyKey_YCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_709_2);
        }
        // SAFETY: the session is live.
        let s = unsafe { VTCompressionSessionPrepareToEncodeFrames(session.0) };
        if s != 0 {
            return Err(fail("get ready", s));
        }
        Ok(Self {
            session,
            sink,
            input: cfg.input,
            height: cfg.height as usize,
            frame_us: (1_000_000.0 / fps).round() as i64,
            cfg: *cfg,
            rate_now: (cfg.bitrate_kbps, cfg.rate),
            lost: false,
            prior_header: None,
            rebuilt_at: std::collections::VecDeque::new(),
        })
    }

    /// A new bitrate for what's encoded from now on.
    pub(crate) fn set_rate(&mut self, kbps: u32, rate: RateControl) -> Result<(), AvError> {
        self.rate_now = (kbps, rate);
        match set_rate(&self.session, kbps, rate) {
            // A session the system took back can't be told: the new bitrate is kept, and the
            // session made again on the next frame starts with it.
            Err(_) if self.session_gone() => Ok(()),
            other => other,
        }
    }

    /// After a rebuilt session: true once, when its codec header (SPS/PPS) is known and differs
    /// from the lost session's, so the stream must send a new sequence header before its
    /// keyframe. False while it isn't known yet (asked again next frame) and when it's the same.
    pub(crate) fn header_changed(&mut self) -> bool {
        let Some(prior) = self.prior_header.as_ref() else { return false };
        let Some(now) = self.header() else { return false };
        let changed = *prior != now;
        self.prior_header = None;
        changed
    }

    pub(crate) fn header(&self) -> Option<Vec<u8>> {
        self.sink.lock().ok().and_then(|s| s.header.clone())
    }

    pub(crate) fn encode(&mut self, frame: Frame<'_>, pts_us: i64, key: bool) -> Result<Vec<VideoPacket>, AvError> {
        self.lost = false;
        match self.encode_once(frame, pts_us, key) {
            Err(first) if self.lost => {
                let now = std::time::Instant::now();
                while self.rebuilt_at.front().is_some_and(|t| now.duration_since(*t) > REBUILD_WINDOW) {
                    self.rebuilt_at.pop_front();
                }
                if self.rebuilt_at.len() >= REBUILDS {
                    self.lost = false;
                    return Err(AvError::Encoder(format!(
                        "the Mac's video encoder keeps stopping ({REBUILDS} restarts in {} s): {first}",
                        REBUILD_WINDOW.as_secs()
                    )));
                }
                // The system took the session back: a new one, and this frame goes in as a
                // keyframe so the stream picks up at once. One try: a second loss is a real fault.
                tracing::warn!(error = %first, "the Mac's video encoder session was lost; making a new one");
                let mut fresh = Encoder::new(&self.cfg)?;
                set_rate(&fresh.session, self.rate_now.0, self.rate_now.1)?;
                fresh.rate_now = self.rate_now;
                // Kept to see whether the new session's parameter sets differ from what the
                // viewers' decoders were given (`header_changed`).
                fresh.prior_header = self.header().or_else(|| self.prior_header.take());
                fresh.rebuilt_at = std::mem::take(&mut self.rebuilt_at);
                fresh.rebuilt_at.push_back(now);
                std::mem::swap(self, &mut fresh);
                self.lost = false;
                self.encode_once(frame, pts_us, true)
            }
            other => other,
        }
    }

    fn encode_once(&mut self, frame: Frame<'_>, pts_us: i64, key: bool) -> Result<Vec<VideoPacket>, AvError> {
        // SAFETY: the session is live; the pool belongs to it (not retained: "Get").
        let pool = unsafe { VTCompressionSessionGetPixelBufferPool(self.session.0) };
        if pool.is_null() {
            self.lost = true;
            return Err(AvError::Encoder("the Mac's video encoder lost its frame pool".into()));
        }
        let mut buf: CFTypeRef = ptr::null();
        // SAFETY: the pool is live; the new buffer is +1.
        let s = unsafe { CVPixelBufferPoolCreatePixelBuffer(ptr::null(), pool, &raw mut buf) };
        let buf = Owned::new(buf).ok_or_else(|| fail("make a frame", s))?;
        self.fill(buf.0, frame)?;
        // SAFETY: CoreVideo's constant keys and values; the buffer is live.
        unsafe {
            CVBufferSetAttachment(
                buf.0,
                kCVImageBufferColorPrimariesKey,
                kCVImageBufferColorPrimaries_ITU_R_709_2,
                PROPAGATE,
            );
            CVBufferSetAttachment(
                buf.0,
                kCVImageBufferTransferFunctionKey,
                kCVImageBufferTransferFunction_ITU_R_709_2,
                PROPAGATE,
            );
            CVBufferSetAttachment(
                buf.0,
                kCVImageBufferYCbCrMatrixKey,
                kCVImageBufferYCbCrMatrix_ITU_R_709_2,
                PROPAGATE,
            );
        }
        let props = if key {
            let d = Dict::new().ok_or_else(|| AvError::Encoder("out of memory".into()))?;
            // SAFETY: VideoToolbox's constant key and CF's constant true.
            unsafe { d.set(kVTEncodeFrameOptionKey_ForceKeyFrame, kCFBooleanTrue) };
            Some(d)
        } else {
            None
        };
        let mut flags = 0u32;
        // SAFETY: session and buffer are live; props is a live dictionary or null; flags is ours.
        let s = unsafe {
            VTCompressionSessionEncodeFrame(
                self.session.0,
                buf.0,
                CMTime::us(pts_us),
                CMTime::us(self.frame_us),
                props.as_ref().map_or(ptr::null(), |d| d.0.0),
                ptr::null_mut(),
                &raw mut flags,
            )
        };
        if s != 0 {
            self.lost = recoverable(s);
            return Err(fail("encode a frame", s));
        }
        self.take()
    }

    /// Copies a frame into a pixel buffer, row by row (their strides differ).
    fn fill(&self, buf: CFTypeRef, frame: Frame<'_>) -> Result<(), AvError> {
        // SAFETY: the buffer is live; locked for writing (flags 0) until unlocked below.
        if unsafe { CVPixelBufferLockBaseAddress(buf, 0) } != 0 {
            return Err(AvError::Encoder("a frame couldn't be written".into()));
        }
        let copy = |dst: *mut u8, dst_stride: usize, rows: usize, src: &[u8], src_stride: usize| {
            let n = dst_stride.min(src_stride);
            for r in 0..rows {
                let Some(line) = src.get(r * src_stride..r * src_stride + n) else { break };
                // SAFETY: the locked buffer has `rows` rows of `dst_stride` bytes at `dst`; `n` is
                // at most both strides, and the source line is in bounds (checked above).
                unsafe { ptr::copy_nonoverlapping(line.as_ptr(), dst.add(r * dst_stride), n) };
            }
        };
        match (frame, self.input) {
            (Frame::Bgra { data, stride }, PixelFormat::Bgra) => {
                // SAFETY: the buffer is locked; one plane.
                let (dst, ds, rows) = unsafe {
                    (CVPixelBufferGetBaseAddress(buf), CVPixelBufferGetBytesPerRow(buf), CVPixelBufferGetHeight(buf))
                };
                if !dst.is_null() {
                    copy(dst, ds, rows.min(self.height), data, stride);
                }
            }
            (Frame::Nv12 { y, y_stride, uv, uv_stride }, PixelFormat::Nv12) => {
                for (plane, src, stride) in [(0, y, y_stride), (1, uv, uv_stride)] {
                    // SAFETY: the buffer is locked; NV12 has planes 0 (Y) and 1 (CbCr).
                    let (dst, ds, rows) = unsafe {
                        (
                            CVPixelBufferGetBaseAddressOfPlane(buf, plane),
                            CVPixelBufferGetBytesPerRowOfPlane(buf, plane),
                            CVPixelBufferGetHeightOfPlane(buf, plane),
                        )
                    };
                    if !dst.is_null() {
                        copy(dst, ds, rows, src, stride);
                    }
                }
            }
            _ => {}
        }
        // SAFETY: locked above with the same flags.
        unsafe { CVPixelBufferUnlockBaseAddress(buf, 0) };
        Ok(())
    }

    /// Whether the system has taken the session back (its frame pool is gone with it).
    fn session_gone(&self) -> bool {
        // SAFETY: the session is live (invalidated or not); the pool is only looked at ("Get").
        unsafe { VTCompressionSessionGetPixelBufferPool(self.session.0) }.is_null()
    }

    pub(crate) fn flush(&mut self) -> Result<Vec<VideoPacket>, AvError> {
        // SAFETY: the session is live; an invalid time means "every frame".
        let s = unsafe { VTCompressionSessionCompleteFrames(self.session.0, CMTime::invalid()) };
        if s != 0 {
            // A lost session has nothing left to hand over: stopping an output after the system
            // took it back finishes with what it had, not with an error.
            if recoverable(s) || self.session_gone() {
                tracing::warn!(status = s, "the video encoder's session was lost before it finished");
                return Ok(Vec::new());
            }
            return Err(fail("finish", s));
        }
        self.take()
    }

    /// Tests: the lost session's header isn't what the new one will make.
    #[cfg(test)]
    pub(crate) fn perturb_header_for_test(&mut self) {
        if let Ok(mut sink) = self.sink.lock()
            && let Some(b) = sink.header.as_mut().and_then(|h| h.last_mut())
        {
            *b ^= 0x55;
        }
    }

    /// Tests: the system takes the session back.
    #[cfg(test)]
    pub(crate) fn invalidate_for_test(&mut self) {
        // SAFETY: the session is live; the system does the same when it takes one back.
        unsafe { VTCompressionSessionInvalidate(self.session.0) };
    }

    fn take(&mut self) -> Result<Vec<VideoPacket>, AvError> {
        let mut sink = self.sink.lock().map_err(|_| AvError::Encoder("the video encoder stopped".into()))?;
        if let Some(e) = sink.error.take() {
            self.lost = sink.status.take().is_some_and(recoverable);
            return Err(AvError::Encoder(e));
        }
        Ok(std::mem::take(&mut sink.packets))
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: the session is live; after this no callback runs, so the sink may go.
        unsafe { VTCompressionSessionInvalidate(self.session.0) };
    }
}

/// VideoToolbox's output callback: one encoded frame (or a dropped one, or an error).
extern "C" fn output(refcon: *mut c_void, _frame: *mut c_void, status: OSStatus, flags: u32, sbuf: *const c_void) {
    if refcon.is_null() {
        return;
    }
    // SAFETY: the refcon is the encoder's `Arc<Mutex<Sink>>` payload, alive until the session is
    // invalidated (Drop), after which this isn't called.
    let sink = unsafe { &*refcon.cast::<Mutex<Sink>>() };
    let Ok(mut sink) = sink.lock() else { return };
    if status != 0 {
        sink.error = Some(format!("the Mac's video encoder failed a frame (error {status})"));
        sink.status = Some(status);
        return;
    }
    if flags & DROPPED != 0 {
        // VideoToolbox left this frame out (it was behind): counted, as FFmpeg's are.
        crate::video::note_dropped(1);
        return;
    }
    if sbuf.is_null() {
        return;
    }
    // SAFETY: `sbuf` is a live CMSampleBuffer for the duration of the callback; every "Get" below
    // borrows from it without retaining, and nothing outlives the callback but copied bytes.
    unsafe {
        let block = CMSampleBufferGetDataBuffer(sbuf);
        if block.is_null() {
            return;
        }
        let len = CMBlockBufferGetDataLength(block);
        let mut data = vec![0u8; len];
        if CMBlockBufferCopyDataBytes(block, 0, len, data.as_mut_ptr().cast()) != 0 {
            sink.error = Some("an encoded frame couldn't be read".into());
            return;
        }
        let pts = CMSampleBufferGetPresentationTimeStamp(sbuf).to_us().unwrap_or(0);
        let dts = CMSampleBufferGetDecodeTimeStamp(sbuf).to_us().unwrap_or(pts);
        let mut key = true;
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sbuf, 0);
        if !attachments.is_null() && CFArrayGetCount(attachments) > 0 {
            let d = CFArrayGetValueAtIndex(attachments, 0);
            let not_sync = CFDictionaryGetValue(d, kCMSampleAttachmentKey_NotSync);
            key = not_sync.is_null() || not_sync == kCFBooleanFalse;
        }
        if key && sink.header.is_none() {
            let desc = CMSampleBufferGetFormatDescription(sbuf);
            if !desc.is_null() {
                let atoms = CMFormatDescriptionGetExtension(
                    desc,
                    kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms,
                );
                if !atoms.is_null() {
                    for name in [c"avcC", c"hvcC"] {
                        let Some(k) = string(name) else { continue };
                        let v = CFDictionaryGetValue(atoms, k.0);
                        if !v.is_null() {
                            sink.header = Some(data_bytes(v));
                        }
                    }
                }
            }
        }
        sink.packets.push(VideoPacket { pts_us: pts, dts_us: dts, key, data });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> VideoConfig {
        VideoConfig {
            codec: VideoCodec::H264,
            input: PixelFormat::Nv12,
            width: 320,
            height: 180,
            fps_num: 30,
            fps_den: 1,
            bitrate_kbps: 1500,
            rate: RateControl::Cbr,
            keyframe_secs: 2,
            software: false,
        }
    }

    fn frame(buf: &(Vec<u8>, Vec<u8>)) -> Frame<'_> {
        Frame::Nv12 { y: &buf.0, y_stride: 320, uv: &buf.1, uv_stride: 320 }
    }

    /// The system takes the session away (display sleep, screen lock, the media server restarting:
    /// agy GAPS #4): the encoder makes a new one and goes on with a keyframe, not an error that
    /// ends the stream.
    #[test]
    fn a_session_the_system_invalidated_is_made_again() {
        let Ok(mut e) = Encoder::new(&config()) else { return };
        let pix = (vec![90u8; 320 * 180], vec![128u8; 320 * 90]);
        let mut keys = 0;
        for i in 0..5 {
            e.encode(frame(&pix), i * 33_333, i == 0).unwrap_or_else(|x| panic!("before: {x}"));
        }
        // SAFETY: the session is live; the system does the same when it takes one back.
        unsafe { VTCompressionSessionInvalidate(e.session.0) };
        let mut failed = None;
        for i in 5..40 {
            match e.encode(frame(&pix), i * 33_333, false) {
                Ok(packets) => keys += packets.iter().filter(|p| p.key).count(),
                Err(x) => {
                    failed = Some(x.to_string());
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(failed, None, "an invalidated session must be recreated, not fail the output");
        assert!(keys >= 1, "the new session starts with a keyframe");
    }

    /// An encoder that keeps dying isn't rebuilt every frame for ever: three restarts, then the
    /// output stops with its own error (agy review R3).
    #[test]
    fn an_encoder_that_keeps_dying_is_given_up_on() {
        let Ok(mut e) = Encoder::new(&config()) else { return };
        let pix = (vec![90u8; 320 * 180], vec![128u8; 320 * 90]);
        e.encode(frame(&pix), 0, true).unwrap_or_else(|x| panic!("first: {x}"));
        let mut results = Vec::new();
        for i in 1..=5 {
            e.invalidate_for_test();
            results.push(e.encode(frame(&pix), i * 33_333, false).is_ok());
        }
        assert_eq!(results, [true, true, true, false, false], "three rebuilds, then it gives up");
    }

    /// Changing the bitrate, or stopping, on a session the system took back is not an error
    /// (agy review R4/R5).
    #[test]
    fn a_lost_session_still_takes_a_bitrate_and_stops_cleanly() {
        let Ok(mut e) = Encoder::new(&config()) else { return };
        let pix = (vec![90u8; 320 * 180], vec![128u8; 320 * 90]);
        e.encode(frame(&pix), 0, true).unwrap_or_else(|x| panic!("first: {x}"));
        e.invalidate_for_test();
        assert!(e.set_rate(900, RateControl::Cbr).is_ok(), "the bitrate is kept for the new session");
        assert_eq!(e.rate_now.0, 900);
        assert!(e.flush().is_ok(), "stopping after a loss isn't an error");
    }

    #[test]
    fn only_a_lost_session_is_worth_recreating() {
        assert!(recoverable(-12903), "kVTInvalidSessionErr");
        assert!(recoverable(-12911), "kVTVideoEncoderMalfunctionErr");
        assert!(recoverable(-12915), "kVTVideoEncoderNotAvailableNowErr");
        assert!(!recoverable(0));
        assert!(!recoverable(-12902), "a bad property won't mend with a new session");
    }
}
