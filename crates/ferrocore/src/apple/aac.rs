//! AAC-LC on AudioToolbox's encoder: one AudioConverter, fed 1024 samples a call.

use std::ffi::c_void;
use std::ptr;

use super::OSStatus;
use crate::AvError;
use crate::audio::{AAC_FRAME, AudioConfig};

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct Asbd {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Debug)]
struct AudioBuffer {
    channels: u32,
    size: u32,
    data: *mut c_void,
}

#[repr(C)]
#[derive(Debug)]
struct AudioBufferList {
    count: u32,
    buffers: [AudioBuffer; 1],
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct PacketDescription {
    start: i64,
    variable_frames: u32,
    size: u32,
}

type InputProc =
    extern "C" fn(*const c_void, *mut u32, *mut AudioBufferList, *mut *mut PacketDescription, *mut c_void) -> OSStatus;

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioConverterNew(from: *const Asbd, to: *const Asbd, out: *mut *const c_void) -> OSStatus;
    fn AudioConverterDispose(c: *const c_void) -> OSStatus;
    fn AudioConverterSetProperty(c: *const c_void, id: u32, size: u32, data: *const c_void) -> OSStatus;
    fn AudioConverterGetProperty(c: *const c_void, id: u32, size: *mut u32, data: *mut c_void) -> OSStatus;
    fn AudioConverterFillComplexBuffer(
        c: *const c_void,
        input: InputProc,
        user: *mut c_void,
        packets: *mut u32,
        out: *mut AudioBufferList,
        descs: *mut PacketDescription,
    ) -> OSStatus;
}

const LPCM: u32 = u32::from_be_bytes(*b"lpcm");
const AAC: u32 = u32::from_be_bytes(*b"aac ");
/// kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked.
const FLOAT_PACKED: u32 = 1 | 8;
/// kAudioConverterEncodeBitRate, kAudioConverterPropertyMaximumOutputPacketSize.
const BITRATE: u32 = u32::from_be_bytes(*b"brat");
const MAX_PACKET: u32 = u32::from_be_bytes(*b"xops");
/// kAudioConverterPrimeInfo: the encoder's start-up delay.
const PRIME_INFO: u32 = u32::from_be_bytes(*b"prim");
/// Our input callback's "that's all for now".
const NO_MORE: OSStatus = 0x6e6f_6d72;

/// The samples one call may take: given once, then "no more".
#[derive(Debug)]
struct Feed {
    data: *const f32,
    frames: u32,
    channels: u32,
    given: bool,
}

#[derive(Debug)]
pub(crate) struct Encoder {
    conv: *const c_void,
    channels: u32,
    max_packet: u32,
    /// Samples (per channel) the encoder puts before the first real one.
    pub(crate) priming: u32,
}

// SAFETY: the converter is only used through `&mut self`; AudioConverters aren't tied to a thread.
unsafe impl Send for Encoder {}

impl Encoder {
    pub(crate) fn new(cfg: &AudioConfig) -> Result<Self, AvError> {
        let from = Asbd {
            sample_rate: f64::from(cfg.sample_rate),
            format_id: LPCM,
            format_flags: FLOAT_PACKED,
            bytes_per_packet: 4 * cfg.channels,
            frames_per_packet: 1,
            bytes_per_frame: 4 * cfg.channels,
            channels_per_frame: cfg.channels,
            bits_per_channel: 32,
            reserved: 0,
        };
        let to = Asbd {
            sample_rate: f64::from(cfg.sample_rate),
            format_id: AAC,
            frames_per_packet: AAC_FRAME as u32,
            channels_per_frame: cfg.channels,
            ..Default::default()
        };
        let mut conv = ptr::null();
        // SAFETY: both descriptions are valid for the call; the converter is ours to dispose.
        let s = unsafe { AudioConverterNew(&raw const from, &raw const to, &raw mut conv) };
        if s != 0 || conv.is_null() {
            return Err(AvError::Encoder(format!("the Mac's AAC encoder didn't start (error {s})")));
        }
        // Owned from here: dropped (disposed) on any early return.
        let mut enc = Self { conv, channels: cfg.channels, max_packet: 0, priming: 0 };
        let bits = cfg.bitrate_kbps * 1000;
        // SAFETY: the converter is live; the property is a u32 read from `bits`.
        let s = unsafe { AudioConverterSetProperty(conv, BITRATE, 4, (&raw const bits).cast()) };
        if s != 0 {
            return Err(AvError::Encoder(format!(
                "the AAC encoder doesn't take {} kbps (error {s})",
                cfg.bitrate_kbps
            )));
        }
        let (mut max, mut size) = (0u32, 4u32);
        // SAFETY: the converter is live; the property is a u32 written into `max`.
        unsafe { AudioConverterGetProperty(conv, MAX_PACKET, &raw mut size, (&raw mut max).cast()) };
        enc.max_packet = max.max(1536);
        // AudioConverterPrimeInfo: leading and trailing frames (u32 each).
        let (mut prime, mut size) = ([0u32; 2], 8u32);
        // SAFETY: the converter is live; the property is two u32s written into `prime`.
        let s = unsafe { AudioConverterGetProperty(conv, PRIME_INFO, &raw mut size, prime.as_mut_ptr().cast()) };
        // Apple's AAC encoder primes 2112 samples when it doesn't say.
        enc.priming = if s == 0 && size == 8 { prime[0] } else { 2112 };
        Ok(enc)
    }

    /// Exactly one AAC frame's samples in → the frames that came out (none while priming).
    pub(crate) fn encode(&mut self, pcm: &[f32]) -> Result<Vec<Vec<u8>>, AvError> {
        let mut feed = Feed {
            data: pcm.as_ptr(),
            frames: (pcm.len() / self.channels as usize) as u32,
            channels: self.channels,
            given: false,
        };
        let mut out = Vec::new();
        loop {
            let mut buf = vec![0u8; self.max_packet as usize];
            let mut list = AudioBufferList {
                count: 1,
                buffers: [AudioBuffer {
                    channels: self.channels,
                    size: self.max_packet,
                    data: buf.as_mut_ptr().cast(),
                }],
            };
            let mut packets = 1u32;
            let mut desc = PacketDescription::default();
            // SAFETY: the converter is live; `feed` (and the samples it points at) outlives the
            // call; the output list points at `buf`, `max_packet` bytes long.
            let s = unsafe {
                fill_complex_aligned(self.conv, &raw mut feed, &raw mut packets, &raw mut list, &raw mut desc)
            };
            if s != 0 && s != NO_MORE {
                return Err(AvError::Encoder(format!("the AAC encoder failed (error {s})")));
            }
            if packets == 0 {
                break;
            }
            buf.truncate(list.buffers[0].size as usize);
            out.push(buf);
            if s == NO_MORE {
                break;
            }
        }
        Ok(out)
    }
}

#[inline(never)]
unsafe fn fill_complex_aligned(
    conv: *const c_void,
    feed: *mut Feed,
    packets: *mut u32,
    out: *mut AudioBufferList,
    descs: *mut PacketDescription,
) -> OSStatus {
    #[repr(C, align(64))]
    struct StackAlign64([u8; 64]);
    let mut align_guard = std::hint::black_box(StackAlign64([0; 64]));
    // SAFETY: caller guarantees `conv`, `feed`, `packets`, `out`, and `descs` are valid for the call.
    let status = unsafe { AudioConverterFillComplexBuffer(conv, input, feed.cast(), packets, out, descs) };
    std::hint::black_box(&mut align_guard);
    status
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: the converter is live and disposed once.
        unsafe { AudioConverterDispose(self.conv) };
    }
}

/// The converter asks for samples: it gets the feed once, then "no more" until the next call.
extern "C" fn input(
    _c: *const c_void,
    packets: *mut u32,
    data: *mut AudioBufferList,
    _d: *mut *mut PacketDescription,
    user: *mut c_void,
) -> OSStatus {
    // SAFETY: `user` is the `Feed` passed to FillComplexBuffer, alive for that call; `packets` and
    // `data` are the converter's, valid for this callback.
    unsafe {
        let feed = &mut *user.cast::<Feed>();
        if feed.given {
            *packets = 0;
            return NO_MORE;
        }
        feed.given = true;
        *packets = feed.frames;
        let b = &mut (*data).buffers[0];
        b.channels = feed.channels;
        b.size = feed.frames * feed.channels * 4;
        b.data = feed.data.cast_mut().cast();
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[inline(never)]
    fn encode_at_pad<const PAD: usize>(blocks: &[Vec<f32>]) -> (usize, Vec<Vec<u8>>) {
        let mut s = [0u8; PAD];
        for (i, b) in s.iter_mut().enumerate() {
            *b = i as u8;
        }
        std::hint::black_box(&mut s);
        let sp_mod_64 = (&raw const s as usize) % 64;
        let cfg = AudioConfig { sample_rate: 48_000, channels: 2, bitrate_kbps: 128 };
        let mut enc = Encoder::new(&cfg).unwrap_or_else(|e| panic!("{e}"));
        let mut out = Vec::new();
        for blk in blocks {
            out.extend(enc.encode(blk).unwrap_or_else(|e| panic!("{e}")));
        }
        (sp_mod_64, out)
    }

    #[inline(never)]
    fn at_stack_depth<F: FnOnce() -> R, R>(depth: usize, f: F) -> R {
        let pad = std::hint::black_box([depth as u8; 16]);
        if depth == 0 {
            let r = f();
            std::hint::black_box(pad);
            r
        } else {
            let r = at_stack_depth(depth - 1, f);
            std::hint::black_box(pad);
            r
        }
    }

    #[test]
    fn encoding_is_identical_across_caller_stack_offsets() {
        let blocks: Vec<Vec<f32>> = (0..16)
            .map(|k| (0..AAC_FRAME * 2).map(|i| ((k * AAC_FRAME * 2 + i) as f32 * 0.05).sin() * 0.35).collect())
            .collect();
        let (_, baseline) = encode_at_pad::<1>(&blocks);
        let mut observed_offsets = std::collections::HashSet::new();
        let mut check = |depth: usize, (off, out): (usize, Vec<Vec<u8>>)| {
            observed_offsets.insert(off);
            assert_eq!(baseline, out, "AAC output differed at depth={depth}, SP%64={off}");
        };
        for depth in 0..4 {
            check(depth, at_stack_depth(depth, || encode_at_pad::<1>(&blocks)));
            check(depth, at_stack_depth(depth, || encode_at_pad::<17>(&blocks)));
            check(depth, at_stack_depth(depth, || encode_at_pad::<33>(&blocks)));
            check(depth, at_stack_depth(depth, || encode_at_pad::<49>(&blocks)));
        }
        assert!(observed_offsets.len() >= 3, "expected distinct caller SP % 64 offsets, got {observed_offsets:?}");
    }
}
