//! H.264 and HEVC on the Mac's media engine (VideoToolbox): real-time, frame reordering (B frames)
//! only when asked (`VideoConfig::bframes`; then packets carry their own decode times and leave a
//! frame or two late), keyframes every N seconds, Rec. 709 colours.

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
    static kVTProfileLevel_H264_Main_AutoLevel: CFStringRef;
    static kVTProfileLevel_H264_Baseline_AutoLevel: CFStringRef;
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
    fn CVPixelBufferGetIOSurface(buf: CFTypeRef) -> CFTypeRef;
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

/// Reorder depth (in frames) used to shift decode timestamps before presentation timestamps so
/// `dts <= pts` (`cts >= 0`) holds in steady state without clamping. VideoToolbox's real-time
/// H.264 and HEVC encoders reorder up to two frames (and B-pyramids up to three).
const REORDER_DEPTH: i64 = 3;

/// What the encoder's callback fills, on VideoToolbox's own thread.
#[derive(Debug, Default)]
struct Sink {
    packets: Vec<VideoPacket>,
    header: Option<Vec<u8>>,
    error: Option<String>,
    /// The status behind `error`, to tell a lost session from a bad frame.
    status: Option<OSStatus>,
    /// Times VideoToolbox has called back (a frame, a drop, an error: anything), to tell a
    /// session that went quiet from one that is working.
    callbacks: u64,
    /// Video rate fraction used to space decode times by exact rational frame intervals.
    fps_num: i64,
    fps_den: i64,
    /// With B-frames: how far decode times are moved back (µs) so none is after its picture's
    /// presentation time (`REORDER_DEPTH` frame intervals).
    dts_shift: i64,
    /// Capture timestamps (µs) of frames submitted to VideoToolbox and not yet output or dropped,
    /// keyed by the monotonic `frame_refcon` id.
    pending: Vec<(usize, i64)>,
    /// Presentation time (µs) anchoring the current continuous decode-order grid, and packets
    /// emitted on that grid so far.
    anchor_pts: Option<i64>,
    decode_idx: i64,
    /// Last decode time given (they must strictly rise).
    last_dts: Option<i64>,
    /// Set once if the last-resort clamp ever has to break steady-state spacing.
    clamp_logged: bool,
    /// Per-session dropped frame count (always recorded locally for `skipped()`).
    dropped: u64,
    /// When false (stress benchmarks), drops stay local and never increment global `DROPPED`.
    record_global_drops: bool,
    /// Tests: the session stops answering (callbacks return at once, as a wedged one's would).
    #[cfg(test)]
    mute: bool,
}

impl Sink {
    fn step_us(&self, frames: i64) -> i64 {
        if self.fps_num > 0 && self.fps_den > 0 {
            (i128::from(frames) * 1_000_000 * i128::from(self.fps_den) / i128::from(self.fps_num)) as i64
        } else if self.dts_shift > 0 {
            frames * (self.dts_shift / REORDER_DEPTH).max(1)
        } else {
            frames * 33_333
        }
    }

    fn warn_clamp_once(&mut self, pts: i64, dts: i64, last: i64) {
        if !self.clamp_logged {
            self.clamp_logged = true;
            tracing::warn!(
                pts_us = pts,
                dts_us = dts,
                last_dts_us = last,
                "VideoToolbox B-frame decode timestamp clamped to stay monotonic"
            );
        }
    }

    fn step_within_headroom(&self, last: i64, min_pending: i64, slots: i64, one_frame: i64) -> Option<i64> {
        (min_pending > last).then(|| {
            let headroom = min_pending - last;
            last + (headroom / slots.max(1)).clamp(1, one_frame).min(headroom)
        })
    }

    /// Computes `dts` in decode order spaced by the frame interval and shifted back by `dts_shift`
    /// so `dts <= pts` holds without clamping in steady state; never alters the frame's real `pts`.
    /// Clamping acts only as a last-resort guard (logged once if it ever fires).
    fn normalize(&mut self, pts: i64, _raw_dts: i64) -> (i64, i64) {
        let one_frame = self.step_us(1).max(1);
        if self.dts_shift <= 0 {
            let mut dts = pts;
            if let Some(last) = self.last_dts
                && dts <= last
            {
                dts = last + 1;
                self.warn_clamp_once(pts, dts, last);
            }
            self.last_dts = Some(dts);
            return (pts, dts);
        }

        // Earliest PTS among this frame and all frames still buffered in VideoToolbox: keeping
        // `dts <= min_pending` guarantees no buffered B-frame can ever arrive with `pts < dts`.
        let min_pending = self.pending.iter().map(|&(_, p)| p).min().map_or(pts, |m| m.min(pts));
        let slots = (self.pending.len() as i64 + 1).max(REORDER_DEPTH + 1);

        let mut dts = if let Some(anchor) = self.anchor_pts {
            self.decode_idx += 1;
            let grid_dts = anchor + self.step_us(self.decode_idx) - self.dts_shift;
            let ideal_dts = min_pending - self.dts_shift;
            if ideal_dts - grid_dts > one_frame {
                // Capture timeline jumped forward across dropped/skipped frames and the reorder
                // window has drained past the gap: re-anchor cleanly onto the post-gap timeline.
                self.anchor_pts = Some(min_pending);
                self.decode_idx = 0;
                ideal_dts
            } else {
                grid_dts
            }
        } else {
            let ideal_dts = min_pending - self.dts_shift;
            match self.last_dts {
                Some(last) if ideal_dts <= last => {
                    // Resumed after `set_min_dts` (encoder reshape/rebuild) where the previous
                    // encoder's DTS sits ahead of `min_pending - dts_shift`: step smoothly inside
                    // `(last, min_pending]` until `ideal_dts` overtakes `last`.
                    if let Some(stepped) = self.step_within_headroom(last, min_pending, slots, one_frame) {
                        stepped
                    } else {
                        let clamped = last + 1;
                        self.warn_clamp_once(pts, clamped, last);
                        clamped
                    }
                }
                _ => {
                    self.anchor_pts = Some(min_pending);
                    self.decode_idx = 0;
                    ideal_dts
                }
            }
        };

        // Guard against `dts` overtaking `min_pending` (e.g. if encoder reorder depth ever exceeds
        // `REORDER_DEPTH` or extreme capture jitter occurs): stay within `(last_dts, min_pending]`.
        if dts > min_pending {
            let clamped = match self.last_dts {
                Some(last) => self.step_within_headroom(last, min_pending, slots, one_frame).unwrap_or(last + 1),
                None => min_pending,
            };
            self.warn_clamp_once(pts, clamped, self.last_dts.unwrap_or(clamped));
            dts = clamped;
            self.anchor_pts = None;
            self.decode_idx = 0;
        }

        // Last-resort monotonicity guard.
        if let Some(last) = self.last_dts
            && dts <= last
        {
            let clamped = self.step_within_headroom(last, min_pending, 4, one_frame).unwrap_or(last + 1);
            self.warn_clamp_once(pts, clamped, last);
            dts = clamped;
            self.anchor_pts = None;
            self.decode_idx = 0;
        }

        self.last_dts = Some(dts);
        (pts, dts)
    }
}

/// Whether the session reorders frames: asked for, and the profile has B-frames (Baseline hasn't).
fn reorder_on(cfg: &VideoConfig) -> bool {
    cfg.bframes && !(cfg.codec == VideoCodec::H264 && cfg.profile == crate::video::H264Profile::Baseline)
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
    /// Frames handed in since VideoToolbox last called back, and since when it has been quiet.
    quiet: (u32, std::time::Instant),
    seen_callbacks: u64,
    /// Frames in a row the pool had none to give.
    pool_misses: u32,
    /// Monotonic id passed as `frame_refcon` so `output` removes each emitted/dropped frame from
    /// `Sink::pending`.
    next_seq: usize,
    /// What each frame costs, logged once a minute (`vt_cost`).
    cost: Box<super::vt_cost::Cost>,
    /// Whether drops also increment global `crate::video::DROPPED`.
    record_global_drops: bool,
    /// When true (bounded benchmark mode), `encode_once_inner` blocks until VT completes each frame.
    sync_per_frame: bool,
}

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceGetID(surface: CFTypeRef) -> u32;
}

/// What one pool miss means.
#[derive(Debug, PartialEq, Eq)]
enum PoolMiss {
    /// Leave this frame out.
    Drop,
    /// Too many in a row: the session isn't giving frames back.
    Wedged,
}

/// Counts a miss: dropped until [`POOL_MISSES`] in a row, then a wedge.
fn pool_miss(misses: &mut u32) -> PoolMiss {
    *misses += 1;
    if *misses >= POOL_MISSES { PoolMiss::Wedged } else { PoolMiss::Drop }
}

/// Pool misses in a row that mean the session is wedged.
const POOL_MISSES: u32 = 30;

/// A session that takes this many frames and this long with no callback at all (no packet, no
/// dropped frame, no error) has wedged: it is treated as lost and built again. (A 1440p60 stream
/// stopped sending at 25 min with rendering fine and nothing in the log.)
const QUIET_FRAMES: u32 = 90;
const QUIET_FOR: std::time::Duration = std::time::Duration::from_millis(1500);

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
        Self::new_inner(cfg, true, false)
    }

    pub(crate) fn new_for_benchmark(cfg: &VideoConfig) -> Result<Self, AvError> {
        Self::new_inner(cfg, false, false)
    }

    pub(crate) fn new_for_benchmark_sync(cfg: &VideoConfig) -> Result<Self, AvError> {
        Self::new_inner(cfg, false, true)
    }

    fn new_inner(cfg: &VideoConfig, record_global_drops: bool, sync_per_frame: bool) -> Result<Self, AvError> {
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
        let sink = Arc::new(Mutex::new(Sink { record_global_drops, ..Sink::default() }));
        // Three frames: the deepest reordering VideoToolbox's real-time H.264 and HEVC use is two.
        // Only container times move, nothing waits.
        if let Ok(mut k) = sink.lock() {
            k.fps_num = i64::from(cfg.fps_num.max(1));
            k.fps_den = i64::from(cfg.fps_den.max(1));
            if reorder_on(cfg) {
                k.dts_shift = k.step_us(REORDER_DEPTH);
            }
        }
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
        let gop = float(f64::from(cfg.gop_secs())).ok_or_else(none)?;
        let rate = float(fps).ok_or_else(none)?;
        // Baseline has no B-frames: reordering stays off there whatever was asked.
        let reorder = reorder_on(cfg);
        // SAFETY: reading VideoToolbox's and CoreVideo's constant CFStrings and CF booleans.
        let required = unsafe {
            [
                (kVTCompressionPropertyKey_RealTime, kCFBooleanTrue, "run in real time"),
                (
                    kVTCompressionPropertyKey_AllowFrameReordering,
                    if reorder { kCFBooleanTrue } else { kCFBooleanFalse },
                    "set B-frames",
                ),
                (kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, gop.0, "set the keyframe interval"),
                (
                    kVTCompressionPropertyKey_ProfileLevel,
                    if cfg.codec == VideoCodec::H264 {
                        match cfg.profile {
                            crate::video::H264Profile::Baseline => kVTProfileLevel_H264_Baseline_AutoLevel,
                            crate::video::H264Profile::Main => kVTProfileLevel_H264_Main_AutoLevel,
                            crate::video::H264Profile::High => kVTProfileLevel_H264_High_AutoLevel,
                        }
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
        let key_frames = int(i32::try_from(cfg.gop_secs() * fps.round().max(1.0) as u32).unwrap_or(i32::MAX));
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
            quiet: (0, std::time::Instant::now()),
            seen_callbacks: 0,
            pool_misses: 0,
            next_seq: 1,
            cost: Box::new(super::vt_cost::Cost::new()),
            record_global_drops,
            sync_per_frame,
        })
    }

    pub(crate) fn skipped(&self) -> u64 {
        self.sink.lock().map_or(0, |s| s.dropped)
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
        let own = self.sink.lock().ok().and_then(|s| s.header.clone());
        let Some(now) = own else { return false };
        let changed = *prior != now;
        self.prior_header = None;
        changed
    }

    /// The codec header: the session's own, or (right after a rebuild, until the new session
    /// has made its first frame) the lost session's, so nothing that asks in that moment (a
    /// replay save) finds none.
    pub(crate) fn header(&self) -> Option<Vec<u8>> {
        self.sink.lock().ok().and_then(|s| s.header.clone()).or_else(|| self.prior_header.clone())
    }

    pub(crate) fn dts_shift(&self) -> i64 {
        self.sink.lock().map_or(0, |s| s.dts_shift)
    }

    pub(crate) fn set_min_dts(&mut self, last_dts: i64) {
        if let Ok(mut s) = self.sink.lock() {
            s.last_dts = Some(s.last_dts.map_or(last_dts, |prev| prev.max(last_dts)));
            s.anchor_pts = None;
            s.decode_idx = 0;
        }
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
                let mut fresh = Self::new_inner(&self.cfg, self.record_global_drops, self.sync_per_frame)?;
                set_rate(&fresh.session, self.rate_now.0, self.rate_now.1)?;
                fresh.rate_now = self.rate_now;
                // Kept to see whether the new session's parameter sets differ from what the
                // viewers' decoders were given (`header_changed`).
                fresh.prior_header = self.header().or_else(|| self.prior_header.take());
                fresh.rebuilt_at = std::mem::take(&mut self.rebuilt_at);
                fresh.rebuilt_at.push_back(now);
                // Decode times go on rising across the new session.
                let last = self.sink.lock().ok().and_then(|k| k.last_dts);
                if let Ok(mut k) = fresh.sink.lock() {
                    k.last_dts = last;
                }
                std::mem::swap(self, &mut fresh);
                self.lost = false;
                self.encode_once(frame, pts_us, true)
            }
            other => other,
        }
    }

    /// `encode_once_inner`, timed for the once-a-minute `vt_cost` line.
    fn encode_once(&mut self, frame: Frame<'_>, pts_us: i64, key: bool) -> Result<Vec<VideoPacket>, AvError> {
        let t0 = std::time::Instant::now();
        let out = self.encode_once_inner(frame, pts_us, key);
        self.cost.encode(t0.elapsed());
        self.cost.tick(std::time::Instant::now());
        out
    }

    fn encode_once_inner(&mut self, frame: Frame<'_>, pts_us: i64, key: bool) -> Result<Vec<VideoPacket>, AvError> {
        // Has the session gone quiet? Frames go in, nothing ever comes back.
        let calls = self.sink.lock().map_or(self.seen_callbacks, |s| s.callbacks);
        if calls != self.seen_callbacks {
            self.seen_callbacks = calls;
            self.quiet = (0, std::time::Instant::now());
        } else {
            self.quiet.0 += 1;
            if self.quiet.0 >= QUIET_FRAMES && self.quiet.1.elapsed() >= QUIET_FOR {
                self.lost = true;
                return Err(AvError::Encoder(format!(
                    "the Mac's video encoder stopped answering ({} frames, {:.1} s)",
                    self.quiet.0,
                    self.quiet.1.elapsed().as_secs_f32()
                )));
            }
        }
        // A frame the card drew into a pooled surface goes in as it is (its colour tags were set
        // when it was taken); any other is copied into a buffer of the session's own pool.
        let owned;
        let buf: CFTypeRef = match frame {
            Frame::Surface(px) if self.input == PixelFormat::Nv12 => px.raw(),
            _ => {
                // SAFETY: the session is live; the pool belongs to it (not retained: "Get").
                let pool = unsafe { VTCompressionSessionGetPixelBufferPool(self.session.0) };
                if pool.is_null() {
                    self.lost = true;
                    return Err(AvError::Encoder("the Mac's video encoder lost its frame pool".into()));
                }
                let mut buf: CFTypeRef = ptr::null();
                // SAFETY: the pool is live; the new buffer is +1.
                let s = unsafe { CVPixelBufferPoolCreatePixelBuffer(ptr::null(), pool, &raw mut buf) };
                let Some(b) = Owned::new(buf) else {
                    // No frame left in the session's pool: it isn't getting them back (a session that
                    // stopped answering). A few misses are a busy moment; many in a row is a wedge.
                    match pool_miss(&mut self.pool_misses) {
                        // A busy moment: this frame is left out and counted, the output goes on (before,
                        // one miss tore the output down for a second and made its encoders again).
                        PoolMiss::Drop => {
                            if let Ok(mut k) = self.sink.lock() {
                                k.dropped += 1;
                            }
                            if self.record_global_drops {
                                crate::video::note_dropped(1);
                            }
                            return Ok(Vec::new());
                        }
                        PoolMiss::Wedged => {
                            self.lost = true;
                            return Err(fail("make a frame", s));
                        }
                    }
                };
                self.pool_misses = 0;
                // SAFETY: the buffer is live; the surface is only read for its id ("Get": not retained).
                let surface = unsafe { CVPixelBufferGetIOSurface(b.0) };
                // SAFETY: a live IOSurface, or null (then 0).
                self.cost.buffer(if surface.is_null() { 0 } else { unsafe { IOSurfaceGetID(surface) as usize } });
                let (faults0, t_fill) = (super::vt_cost::process_faults(), std::time::Instant::now());
                self.fill(b.0, frame)?;
                self.cost.fill(t_fill.elapsed(), super::vt_cost::process_faults().saturating_sub(faults0));
                // SAFETY: CoreVideo's constant keys and values; the buffer is live and ours alone.
                unsafe {
                    CVBufferSetAttachment(
                        b.0,
                        kCVImageBufferColorPrimariesKey,
                        kCVImageBufferColorPrimaries_ITU_R_709_2,
                        PROPAGATE,
                    );
                    CVBufferSetAttachment(
                        b.0,
                        kCVImageBufferTransferFunctionKey,
                        kCVImageBufferTransferFunction_ITU_R_709_2,
                        PROPAGATE,
                    );
                    CVBufferSetAttachment(
                        b.0,
                        kCVImageBufferYCbCrMatrixKey,
                        kCVImageBufferYCbCrMatrix_ITU_R_709_2,
                        PROPAGATE,
                    );
                }
                owned = b;
                owned.0
            }
        };
        let props = if key {
            let d = Dict::new().ok_or_else(|| AvError::Encoder("out of memory".into()))?;
            // SAFETY: VideoToolbox's constant key and CF's constant true.
            unsafe { d.set(kVTEncodeFrameOptionKey_ForceKeyFrame, kCFBooleanTrue) };
            Some(d)
        } else {
            None
        };
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1).max(1);
        if let Ok(mut sink) = self.sink.lock() {
            sink.pending.push((seq, pts_us));
        }
        let mut flags = 0u32;
        // SAFETY: session and buffer are live; props is a live dictionary or null; flags is ours;
        // `frame_refcon` carries `seq` as an opaque token, never dereferenced.
        let s = unsafe {
            VTCompressionSessionEncodeFrame(
                self.session.0,
                buf,
                CMTime::us(pts_us),
                CMTime::us(self.frame_us),
                props.as_ref().map_or(ptr::null(), |d| d.0.0),
                ptr::without_provenance_mut(seq),
                &raw mut flags,
            )
        };
        if s != 0 {
            if let Ok(mut sink) = self.sink.lock()
                && let Some(pos) = sink.pending.iter().position(|(id, _)| *id == seq)
            {
                sink.pending.swap_remove(pos);
            }
            self.lost = recoverable(s);
            return Err(fail("encode a frame", s));
        }
        if self.sync_per_frame {
            // SAFETY: the session is live; completing with `CMTime::invalid()` blocks until the
            // just-submitted frame completes encoding and fires `output()` in synchronous benchmark mode.
            let cs = unsafe { VTCompressionSessionCompleteFrames(self.session.0, CMTime::invalid()) };
            if cs != 0 && !recoverable(cs) && !self.session_gone() {
                return Err(fail("complete frame", cs));
            }
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
            crate::video::note_fill_bytes((rows * n) as u64);
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
            #[cfg(target_os = "macos")]
            (Frame::Surface(px), PixelFormat::Nv12) => {
                // Not reached by `encode_once_inner` (it takes a surface as it is); kept so a
                // surface can still be copied into a session buffer.
                let _ = px.with_planes(|y, ys, uv, uvs| {
                    for (plane, src, stride) in [(0, y, ys), (1, uv, uvs)] {
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
                });
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

    /// Tests: the session stops answering.
    #[cfg(test)]
    pub(crate) fn mute_for_test(&mut self) {
        if let Ok(mut sink) = self.sink.lock() {
            sink.mute = true;
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
extern "C" fn output(refcon: *mut c_void, frame: *mut c_void, status: OSStatus, flags: u32, sbuf: *const c_void) {
    if refcon.is_null() {
        return;
    }
    // SAFETY: the refcon is the encoder's `Arc<Mutex<Sink>>` payload, alive until the session is
    // invalidated (Drop), after which this isn't called.
    let sink = unsafe { &*refcon.cast::<Mutex<Sink>>() };
    let Ok(mut sink) = sink.lock() else { return };
    #[cfg(test)]
    if sink.mute {
        return;
    }
    let seq = frame as usize;
    if seq != 0
        && let Some(pos) = sink.pending.iter().position(|(id, _)| *id == seq)
    {
        sink.pending.swap_remove(pos);
    }
    sink.callbacks += 1;
    if status != 0 {
        sink.error = Some(format!("the Mac's video encoder failed a frame (error {status})"));
        sink.status = Some(status);
        return;
    }
    if flags & DROPPED != 0 {
        // VideoToolbox left this frame out (it was behind): always track locally; only increment
        // global live stats when not running an isolated stress benchmark (H1).
        sink.dropped += 1;
        if sink.record_global_drops {
            crate::video::note_dropped(1);
        }
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
        let raw_pts = CMSampleBufferGetPresentationTimeStamp(sbuf).to_us().unwrap_or(0);
        let raw_dts = CMSampleBufferGetDecodeTimeStamp(sbuf).to_us().unwrap_or(raw_pts);
        let (pts, dts) = sink.normalize(raw_pts, raw_dts);
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
            profile: crate::video::H264Profile::High,
            bframes: false,
            preset: None,
            tune: None,
        }
    }

    fn frame(buf: &(Vec<u8>, Vec<u8>)) -> Frame<'_> {
        Frame::Nv12 { y: &buf.0, y_stride: 320, uv: &buf.1, uv_stride: 320 }
    }

    /// The system takes the session away (display sleep, screen lock, the media server restarting:
    /// GAPS #4): the encoder makes a new one and goes on with a keyframe, not an error that
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
    /// output stops with its own error (review R3).
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
    /// (review R4/R5).
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

    /// Right after a rebuild, before the new session has made a frame, the codec header is the
    /// lost session's, not none (a replay save at that moment would find nothing; review R6).
    #[test]
    fn the_header_is_never_none_after_a_rebuild() {
        let Ok(mut e) = Encoder::new(&config()) else { return };
        let pix = (vec![90u8; 320 * 180], vec![128u8; 320 * 90]);
        for i in 0..5 {
            e.encode(frame(&pix), i * 33_333, i == 0).unwrap_or_else(|x| panic!("{x}"));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(e.header().is_some());
        e.invalidate_for_test();
        e.encode(frame(&pix), 6 * 33_333, false).unwrap_or_else(|x| panic!("{x}"));
        assert!(e.header().is_some(), "the lost session's header stands in");
    }

    /// A session that accepts frames and never answers (no packet, no drop, no error) is built
    /// again instead of silently ending the stream (the 25 min stall in a soak).
    #[test]
    fn a_session_that_goes_quiet_is_built_again_and_packets_flow() {
        let Ok(mut e) = Encoder::new(&config()) else { return };
        let pix = (vec![90u8; 320 * 180], vec![128u8; 320 * 90]);
        for i in 0..5 {
            e.encode(frame(&pix), i * 33_333, i == 0).unwrap_or_else(|x| panic!("{x}"));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        e.mute_for_test();
        let mut packets_after = 0;
        let mut failed = None;
        for i in 5..400 {
            match e.encode(frame(&pix), i * 33_333, false) {
                Ok(p) => packets_after += p.len(),
                Err(x) => {
                    failed = Some(x.to_string());
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            if packets_after > 3 {
                break;
            }
        }
        assert_eq!(failed, None, "a quiet session must be rebuilt, not fail the output");
        assert!(packets_after > 3, "packets flow again after the rebuild: {packets_after}");
    }

    /// A few pool misses drop frames; 30 in a row are a wedge (D1F-05: the first one used to be an
    /// error, so the 30 were unreachable).
    #[test]
    fn a_few_pool_misses_drop_frames_and_thirty_are_a_wedge() {
        let mut misses = 0;
        for _ in 0..(POOL_MISSES - 1) {
            assert_eq!(pool_miss(&mut misses), PoolMiss::Drop);
        }
        assert_eq!(pool_miss(&mut misses), PoolMiss::Wedged);
    }

    #[test]
    fn only_a_lost_session_is_worth_recreating() {
        assert!(recoverable(-12903), "kVTInvalidSessionErr");
        assert!(recoverable(-12911), "kVTVideoEncoderMalfunctionErr");
        assert!(recoverable(-12915), "kVTVideoEncoderNotAvailableNowErr");
        assert!(!recoverable(0));
        assert!(!recoverable(-12902), "a bad property won't mend with a new session");
    }

    /// B-frames on: packets come back with their own times (decode never after display, decode
    /// order steady), and pictures really are reordered. Off: nothing is.
    #[test]
    fn b_frames_reorder_only_when_asked() {
        for (codec, on) in
            [(VideoCodec::H264, false), (VideoCodec::H264, true), (VideoCodec::Hevc, false), (VideoCodec::Hevc, true)]
        {
            let Ok(mut e) = Encoder::new(&VideoConfig { bframes: on, codec, ..config() }) else { continue };
            let mut out = Vec::new();
            for i in 0..90i64 {
                // Moving picture: still frames would not need B-frames at all.
                let y: Vec<u8> = (0..320 * 180).map(|p| ((p + i as usize * 7) % 251) as u8).collect();
                let pix = (y, vec![128u8; 320 * 90]);
                out.extend(e.encode(frame(&pix), i * 33_333, i == 0).unwrap_or_else(|er| panic!("{er}")));
            }
            out.extend(e.flush().unwrap_or_else(|er| panic!("{er}")));
            assert_eq!(out.len(), 90, "every frame comes out (bframes {on})");
            assert!(out[0].key, "starts on a keyframe");
            assert!(out.windows(2).all(|w| w[1].dts_us > w[0].dts_us), "decode times rise (bframes {on})");
            assert!(out.iter().all(|p| p.dts_us <= p.pts_us), "never shown before decoded (bframes {on})");
            let reordered = out.windows(2).filter(|w| w[1].pts_us < w[0].pts_us).count();
            if on {
                assert!(reordered > 0, "B-frames reorder pictures");
                assert!(out.iter().any(|p| p.pts_us > p.dts_us), "composition offsets exist");
            } else {
                assert_eq!(reordered, 0);
                assert!(out.iter().all(|p| p.pts_us == p.dts_us));
            }
            // Every picture exactly once.
            let mut pts: Vec<i64> = out.iter().map(|p| p.pts_us).collect();
            pts.sort_unstable();
            assert_eq!(pts, (0..90).map(|i| i * 33_333).collect::<Vec<i64>>());
        }
    }

    #[test]
    fn vt_bframes_dts_validity_and_monotonic_cts_non_negative() {
        for codec in [VideoCodec::H264, VideoCodec::Hevc] {
            let Ok(mut e) = Encoder::new(&VideoConfig { bframes: true, codec, ..config() }) else { continue };
            let mut out = Vec::new();
            for i in 0..45i64 {
                let y: Vec<u8> = (0..320 * 180).map(|p| ((p + i as usize * 7) % 251) as u8).collect();
                let pix = (y, vec![128u8; 320 * 90]);
                out.extend(e.encode(frame(&pix), i * 33_333, i == 0).unwrap_or_else(|er| panic!("{er}")));
            }
            out.extend(e.flush().unwrap_or_else(|er| panic!("{er}")));
            assert!(out.windows(2).all(|w| w[1].dts_us > w[0].dts_us), "dts must strictly increase");
            assert!(out.iter().all(|p| p.dts_us <= p.pts_us), "never emit negative cts");
        }
        // Even when an external `last_dts` forces monotonicity, `pts` is never altered and the
        // last-resort clamp logs at most once.
        let mut sink = Sink { dts_shift: 100_000, last_dts: Some(110_000), ..Default::default() };
        let (pts, dts) = sink.normalize(100_000, 100_000);
        assert_eq!(pts, 100_000, "real pts must never be altered");
        assert!(dts > 110_000 && sink.clamp_logged, "pts={pts} dts={dts}");
    }

    #[test]
    fn bframes_set_min_dts_smooth_catchup_never_alters_pts() {
        let mut sink = Sink { fps_num: 30, fps_den: 1, dts_shift: 100_000, ..Default::default() };
        // Previous encoder ended at 433_343; new 30 fps session starts at pts = 500_000 with B-frames.
        sink.last_dts = Some(433_343);
        let input_order = [500_000i64, 533_333, 566_666, 599_999, 633_332, 666_665, 699_998, 733_331];
        let decode_order = [500_000i64, 633_332, 566_666, 533_333, 599_999, 733_331, 666_665, 699_998];
        let mut pending: Vec<(usize, i64)> = input_order.iter().enumerate().map(|(i, &p)| (i + 1, p)).collect();
        let mut out = Vec::new();
        for p in decode_order {
            if let Some(pos) = pending.iter().position(|x| x.1 == p) {
                pending.swap_remove(pos);
            }
            sink.pending = pending.clone();
            out.push(sink.normalize(p, p));
        }
        assert_eq!(out.iter().map(|&(p, _)| p).collect::<Vec<_>>(), decode_order);
        assert!(out.iter().all(|&(p, d)| d <= p), "cts >= 0 across reshape catchup: {out:?}");
        assert!(out[0].1 > 433_343, "stays strictly above old last_dts");
        assert!(out.windows(2).all(|w| w[1].1 > w[0].1), "dts strictly rises: {out:?}");
        assert!(!sink.clamp_logged, "smooth catchup does not trip the last-resort clamp");
    }

    /// Two or more GOPs at 60 fps (and 60000/1001) with B-frames: every output picture keeps its
    /// exact input PTS, decode timestamps stay <= PTS (cts >= 0), and consecutive DTS step by ~one
    /// frame interval across keyframes with no 1 ms clamp steps.
    #[test]
    fn bframes_60fps_two_gops_preserve_pts_and_steady_dts_spacing() {
        for (codec, fps_num, fps_den) in
            [(VideoCodec::H264, 60, 1), (VideoCodec::Hevc, 60, 1), (VideoCodec::H264, 60_000, 1001)]
        {
            let frame_us = (1_000_000.0 * f64::from(fps_den) / f64::from(fps_num)).round() as i64;
            let cfg = VideoConfig {
                codec,
                width: 640,
                height: 360,
                fps_num,
                fps_den,
                bitrate_kbps: 3000,
                keyframe_secs: 1,
                bframes: true,
                ..config()
            };
            let Ok(mut e) = Encoder::new(&cfg) else { continue };
            let mut out = Vec::new();
            let mut input_pts = Vec::with_capacity(180);
            let y_base = vec![90u8; 640 * 360];
            let uv_base = vec![128u8; 640 * 180];
            for i in 0..180i64 {
                let pts = (i128::from(i) * 1_000_000 * i128::from(fps_den) / i128::from(fps_num)) as i64;
                input_pts.push(pts);
                let mut y = y_base.clone();
                let stripe = ((i as usize * 37) % 360) * 640;
                let end = (stripe + 640 * 6).min(y.len());
                for b in &mut y[stripe..end] {
                    *b = ((i * 23) % 251) as u8;
                }
                let f = Frame::Nv12 { y: &y, y_stride: 640, uv: &uv_base, uv_stride: 640 };
                out.extend(e.encode(f, pts, i == 0 || i == 90).unwrap_or_else(|er| panic!("{er}")));
            }
            out.extend(e.flush().unwrap_or_else(|er| panic!("{er}")));
            assert_eq!(out.len(), 180, "{codec:?} {fps_num}/{fps_den}: every frame comes out");
            assert!(
                !e.sink.lock().unwrap_or_else(|p| p.into_inner()).clamp_logged,
                "{codec:?} {fps_num}/{fps_den}: steady state must never hit last-resort clamp"
            );

            let keys: Vec<usize> = out.iter().enumerate().filter(|(_, p)| p.key).map(|(i, _)| i).collect();
            assert!(keys.len() >= 2, "{codec:?}: expected 2+ GOPs, got keyframes at {keys:?}");
            assert!(out.windows(2).filter(|w| w[1].pts_us < w[0].pts_us).count() > 0, "{codec:?}: reordered B-frames");

            let mut got_pts: Vec<i64> = out.iter().map(|p| p.pts_us).collect();
            got_pts.sort_unstable();
            assert_eq!(got_pts, input_pts, "{codec:?}: every output PTS must equal its exact input frame time");

            assert!(out.iter().all(|p| p.dts_us <= p.pts_us), "{codec:?}: dts <= pts on every frame");

            let bad_gaps: Vec<_> = out
                .windows(2)
                .enumerate()
                .filter_map(|(idx, w)| {
                    let gap = w[1].dts_us - w[0].dts_us;
                    ((gap - frame_us).abs() > 1_000).then_some((idx, gap, w[0].dts_us, w[1].dts_us, w[1].key))
                })
                .collect();
            assert!(
                bad_gaps.is_empty(),
                "{codec:?} {fps_num}/{fps_den}: DTS steps must stay ~{frame_us} us (keys={keys:?}): {bad_gaps:?}"
            );
        }
    }

    /// Frame skips / dropped ticks in a 60 fps B-frame encode must never alter real capture PTS
    /// or trigger 1 ms clamp steps in decode timestamps.
    #[test]
    fn bframes_60fps_with_frame_skips_preserve_pts_without_1ms_clamping() {
        let frame_us = 16_667i64;
        let cfg = VideoConfig {
            codec: VideoCodec::H264,
            width: 640,
            height: 360,
            fps_num: 60,
            fps_den: 1,
            bitrate_kbps: 3000,
            keyframe_secs: 1,
            bframes: true,
            ..config()
        };
        let Ok(mut e) = Encoder::new(&cfg) else { return };
        let mut out = Vec::new();
        let mut input_pts = Vec::new();
        let mut pts = 0i64;
        let y_base = vec![90u8; 640 * 360];
        let uv_base = vec![128u8; 640 * 180];
        for i in 0..140i64 {
            // Simulate 2-frame engine stalls across multiple GOP phases (pre-skip B-frame buffered behind post-skip frame).
            if matches!(i, 14 | 31 | 63 | 80) {
                pts += frame_us * 3;
            } else if i > 0 {
                pts += frame_us;
            }
            input_pts.push(pts);
            let mut y = y_base.clone();
            let stripe = ((i as usize * 37) % 360) * 640;
            let end = (stripe + 640 * 6).min(y.len());
            for b in &mut y[stripe..end] {
                *b = ((i * 23) % 251) as u8;
            }
            let f = Frame::Nv12 { y: &y, y_stride: 640, uv: &uv_base, uv_stride: 640 };
            out.extend(e.encode(f, pts, i == 0).unwrap_or_else(|er| panic!("{er}")));
        }
        out.extend(e.flush().unwrap_or_else(|er| panic!("{er}")));
        assert_eq!(out.len(), 140);
        assert!(
            !e.sink.lock().unwrap_or_else(|p| p.into_inner()).clamp_logged,
            "frame skips must re-anchor cleanly without hitting the last-resort clamp"
        );

        let mut got_pts: Vec<i64> = out.iter().map(|p| p.pts_us).collect();
        got_pts.sort_unstable();
        assert_eq!(got_pts, input_pts, "PTS must never be altered when frames skip");
        assert!(out.iter().all(|p| p.dts_us <= p.pts_us), "dts <= pts must hold across skips");

        let tiny_or_backwards: Vec<_> = out
            .windows(2)
            .enumerate()
            .filter_map(|(idx, w)| {
                let gap = w[1].dts_us - w[0].dts_us;
                (gap < frame_us / 2).then_some((idx, gap, w[0].pts_us, w[0].dts_us, w[1].pts_us, w[1].dts_us))
            })
            .collect();
        assert!(
            tiny_or_backwards.is_empty(),
            "no 1 ms clamp steps or backwards DTS on skipped frames: {tiny_or_backwards:?}"
        );
    }

    #[test]
    fn h1_vt_benchmark_drops_stay_local_without_polluting_global() {
        let _serial = crate::video::DROPPED_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let Ok(bench_enc) = Encoder::new_for_benchmark(&config()) else { return };
        let before = crate::video::dropped_frames();
        output(Arc::as_ptr(&bench_enc.sink).cast_mut().cast(), ptr::null_mut(), 0, DROPPED, ptr::null());
        let after_bench = crate::video::dropped_frames();
        assert_eq!(bench_enc.skipped(), 1, "benchmark VT drops must be recorded locally");
        assert_eq!(after_bench.saturating_sub(before), 0, "benchmark VT drop must not pollute global dropped_frames()");

        let Ok(live_enc) = Encoder::new(&config()) else { return };
        let before_live = crate::video::dropped_frames();
        output(Arc::as_ptr(&live_enc.sink).cast_mut().cast(), ptr::null_mut(), 0, DROPPED, ptr::null());
        let after_live = crate::video::dropped_frames();
        assert_eq!(live_enc.skipped(), 1);
        assert_eq!(after_live.saturating_sub(before_live), 1);
    }
}
