//! Just enough of FLV's audio and video tags to forward them untouched: which video tag starts a
//! picture a player can decode on its own (a keyframe), and which tags are the codec headers a
//! player needs before anything else. A destination that joins mid-stream gets the headers again
//! and starts at the next keyframe. Covers classic AVC and AAC, and Enhanced RTMP (HEVC, AV1,
//! VP9 video and its audio, as OBS 30+ sends them).

use serde::{Deserialize, Serialize};

/// The video codec, as its header names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
    Vp9,
}

/// Which codec a video header is for (classic AVC/HEVC ids, or Enhanced RTMP's FourCC).
pub fn video_codec(data: &[u8]) -> Option<VideoCodec> {
    let &b = data.first()?;
    if b & 0x80 != 0 {
        return match data.get(1..5)? {
            b"avc1" => Some(VideoCodec::H264),
            b"hvc1" => Some(VideoCodec::Hevc),
            b"av01" => Some(VideoCodec::Av1),
            b"vp09" => Some(VideoCodec::Vp9),
            _ => None,
        };
    }
    match b & 0x0F {
        7 => Some(VideoCodec::H264),
        12 => Some(VideoCodec::Hevc),
        _ => None,
    }
}

/// FLV video frame type 1: a keyframe.
const KEYFRAME: u8 = 1;
/// FLV sound format 10: AAC.
const AAC: u8 = 10;
/// FLV sound format 9: Enhanced RTMP's extended audio header.
const AUDIO_EX_HEADER: u8 = 9;

/// Whether this video tag body is a keyframe (or a codec header, which comes marked as one).
pub fn video_is_keyframe(data: &[u8]) -> bool {
    let Some(&b) = data.first() else { return false };
    // Enhanced RTMP sets the top bit; the frame type is the next three bits either way.
    let frame_type = if b & 0x80 != 0 { (b >> 4) & 0x07 } else { b >> 4 };
    frame_type == KEYFRAME
}

/// Whether this video tag body is the codec's sequence header (decoder configuration).
pub fn video_is_header(data: &[u8]) -> bool {
    let Some(&b) = data.first() else { return false };
    if b & 0x80 != 0 {
        // Enhanced RTMP: packet type in the low four bits, 0 = SequenceStart.
        b & 0x0F == 0
    } else {
        // AVC (codec 7) and the older HEVC tag (12): packet type 0 is the sequence header.
        matches!(b & 0x0F, 7 | 12) && data.get(1) == Some(&0)
    }
}

/// Whether this video tag body is a coded main-track keyframe (not a sequence header or extra track).
pub fn video_is_coded_keyframe(data: &[u8]) -> bool {
    !video_is_header(data) && video_extra_track(data).is_none() && video_is_keyframe(data)
}

/// Whether this audio tag body is the codec's sequence header (AAC's AudioSpecificConfig, or
/// Enhanced RTMP's SequenceStart).
pub fn audio_is_header(data: &[u8]) -> bool {
    let Some(&b) = data.first() else { return false };
    match b >> 4 {
        AAC => data.get(1) == Some(&0),
        AUDIO_EX_HEADER => b & 0x0F == 0,
        _ => false,
    }
}

/// Enhanced RTMP's multitrack audio packet type, and its "one track" kind.
const AUDIO_MULTITRACK: u8 = 5;
const ONE_TRACK: u8 = 0x00;

/// An extra audio track (OBS sends Twitch's VOD track so: Enhanced RTMP multitrack audio, one
/// track a packet, `trackId` after the FourCC): its track number and whether this is that track's
/// codec header. None for the main track (0) and everything else.
pub fn audio_extra_track(data: &[u8]) -> Option<(u8, bool)> {
    let &b = data.first()?;
    if b >> 4 != AUDIO_EX_HEADER || b & 0x0F != AUDIO_MULTITRACK {
        return None;
    }
    let &kind = data.get(1)?;
    if kind & 0xF0 != ONE_TRACK {
        return None;
    }
    let &track = data.get(6)?;
    (track != 0).then_some((track, kind & 0x0F == 0))
}

/// Enhanced RTMP's video packet types this writes: SequenceStart (the codec header),
/// CodedFrames (with a composition time), CodedFramesX (none: pts is dts), Multitrack.
pub const VIDEO_SEQUENCE_START: u8 = 0;
pub const VIDEO_CODED_FRAMES: u8 = 1;
pub const VIDEO_CODED_FRAMES_X: u8 = 3;
const VIDEO_MULTITRACK: u8 = 6;

/// An extra video track's tag (Twitch Enhanced Broadcasting: Enhanced RTMP multitrack video, one
/// track a packet, as OBS's `flv_packet_ex` writes it): `track` 1 or more, `kind` one of the
/// packet types above, `cts` the composition time in ms (CodedFrames only).
pub fn video_track_tag(track: u8, keyframe: bool, kind: u8, fourcc: &[u8; 4], cts: i32, data: &[u8]) -> Vec<u8> {
    let frame = if keyframe { 0x10 } else { 0x20 };
    let mut out = Vec::with_capacity(data.len() + 10);
    out.push(0x80 | frame | VIDEO_MULTITRACK);
    out.push(ONE_TRACK | (kind & 0x0F));
    out.extend_from_slice(fourcc);
    out.push(track);
    if kind == VIDEO_CODED_FRAMES {
        out.extend_from_slice(&cts.to_be_bytes()[1..]);
    }
    out.extend_from_slice(data);
    out
}

/// An extra video track's packet: its track number and whether it's that track's codec header.
/// None for the main track (0) and everything else. A destination that isn't Twitch's
/// Enhanced Broadcasting drops these.
pub fn video_extra_track(data: &[u8]) -> Option<(u8, bool)> {
    let &b = data.first()?;
    if b & 0x80 == 0 || b & 0x0F != VIDEO_MULTITRACK {
        return None;
    }
    let &kind = data.get(1)?;
    if kind & 0xF0 != ONE_TRACK {
        return None;
    }
    let &track = data.get(6)?;
    (track != 0).then_some((track, kind & 0x0F == VIDEO_SEQUENCE_START))
}

/// Extract the `AVCDecoderConfigurationRecord` (`avcC`) payload from an FLV H.264 video
/// sequence header tag (classic AVC codec id 7, or Enhanced RTMP `avc1` SequenceStart).
fn avcc_from_video_header(data: &[u8]) -> Option<&[u8]> {
    let &b = data.first()?;
    if b & 0x80 != 0 {
        if b & 0x0F == VIDEO_SEQUENCE_START && data.get(1..5) == Some(b"avc1") {
            return data.get(5..);
        }
        return None;
    }
    if b & 0x0F == 7 && data.get(1) == Some(&0) {
        return data.get(5..);
    }
    None
}

/// Coded display dimensions `(width, height)` parsed from the first valid H.264 SPS inside an
/// FLV AVC sequence header (`avcC`), including `frame_cropping_flag` offsets (e.g. 1088 -> 1080).
pub fn avcc_sps_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let avcc = avcc_from_video_header(data)?;
    let num_sps = usize::from(*avcc.get(5)? & 0x1F);
    let mut at = 6;
    for _ in 0..num_sps {
        let len = usize::from(u16::from_be_bytes([*avcc.get(at)?, *avcc.get(at + 1)?]));
        let nal = avcc.get(at + 2..at + 2 + len)?;
        at += 2 + len;
        if let Some(&hdr) = nal.first()
            && hdr & 0x1F == 7
            && let Some(dims) = sps_rbsp_dimensions(&nal[1..])
        {
            return Some(dims);
        }
    }
    None
}

fn rbsp_from_nal_payload(bytes: &[u8]) -> Vec<u8> {
    let mut rbsp = Vec::with_capacity(bytes.len());
    let mut zeros = 0u8;
    for &b in bytes {
        if zeros == 2 && b == 0x03 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros.saturating_add(1).min(2) } else { 0 };
        rbsp.push(b);
    }
    rbsp
}

struct BitReader<'a> {
    bytes: &'a [u8],
    byte: usize,
    bit: u8,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, byte: 0, bit: 0 }
    }

    fn read_bit(&mut self) -> Option<u32> {
        let b = *self.bytes.get(self.byte)?;
        let out = u32::from((b >> (7 - self.bit)) & 1);
        self.bit += 1;
        if self.bit == 8 {
            self.bit = 0;
            self.byte += 1;
        }
        Some(out)
    }

    fn read_bits(&mut self, count: u8) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..count {
            v = (v << 1) | self.read_bit()?;
        }
        Some(v)
    }

    fn read_ue(&mut self) -> Option<u32> {
        let mut zeros = 0u8;
        while self.read_bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let mut suffix = 0u64;
        for _ in 0..zeros {
            suffix = (suffix << 1) | u64::from(self.read_bit()?);
        }
        let val = ((1u64 << zeros) - 1).checked_add(suffix)?;
        u32::try_from(val).ok()
    }

    fn read_se(&mut self) -> Option<i32> {
        let code = self.read_ue()?;
        let mag = i64::from(code.div_ceil(2));
        let signed = if code & 1 == 1 { mag } else { -mag };
        i32::try_from(signed).ok()
    }
}

fn skip_scaling_list(r: &mut BitReader<'_>, size: usize) -> Option<()> {
    let mut last_scale = 8i32;
    let mut next_scale = 8i32;
    for _ in 0..size {
        if next_scale != 0 {
            let delta = r.read_se()?;
            next_scale = (last_scale + delta + 256).rem_euclid(256);
        }
        last_scale = if next_scale == 0 { last_scale } else { next_scale };
    }
    Some(())
}

fn sps_rbsp_dimensions(nal_payload: &[u8]) -> Option<(u32, u32)> {
    let rbsp = rbsp_from_nal_payload(nal_payload);
    let mut r = BitReader::new(&rbsp);
    let profile_idc = r.read_bits(8)?;
    let _constraints = r.read_bits(8)?;
    let _level_idc = r.read_bits(8)?;
    let _sps_id = r.read_ue()?;

    let mut chroma_format_idc = 1u32;
    let mut separate_colour_plane_flag = 0u32;
    if matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma_format_idc = r.read_ue()?;
        if chroma_format_idc > 3 {
            return None;
        }
        if chroma_format_idc == 3 {
            separate_colour_plane_flag = r.read_bit()?;
        }
        let _bit_depth_luma_minus8 = r.read_ue()?;
        let _bit_depth_chroma_minus8 = r.read_ue()?;
        let _qpprime_y_zero_transform = r.read_bit()?;
        if r.read_bit()? == 1 {
            let lists = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..lists {
                if r.read_bit()? == 1 {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }

    let _log2_max_frame_num_minus4 = r.read_ue()?;
    match r.read_ue()? {
        0 => {
            let _log2_max_pic_order_cnt_lsb_minus4 = r.read_ue()?;
        }
        1 => {
            let _delta_pic_order_always_zero = r.read_bit()?;
            let _offset_non_ref = r.read_se()?;
            let _offset_top_to_bottom = r.read_se()?;
            let num_ref_frames_in_cycle = r.read_ue()?;
            if num_ref_frames_in_cycle > 256 {
                return None;
            }
            for _ in 0..num_ref_frames_in_cycle {
                let _offset = r.read_se()?;
            }
        }
        2 => {}
        _ => return None,
    }

    let _max_num_ref_frames = r.read_ue()?;
    let _gaps_in_frame_num_allowed = r.read_bit()?;
    let pic_width_in_mbs_minus1 = r.read_ue()?;
    let pic_height_in_map_units_minus1 = r.read_ue()?;
    let frame_mbs_only_flag = r.read_bit()?;
    if frame_mbs_only_flag == 0 {
        let _mb_adaptive_frame_field = r.read_bit()?;
    }
    let _direct_8x8_inference = r.read_bit()?;
    let frame_cropping_flag = r.read_bit()?;
    let (crop_left, crop_right, crop_top, crop_bottom) =
        if frame_cropping_flag == 1 { (r.read_ue()?, r.read_ue()?, r.read_ue()?, r.read_ue()?) } else { (0, 0, 0, 0) };

    let coded_w = pic_width_in_mbs_minus1.checked_add(1)?.checked_mul(16)?;
    let coded_h =
        pic_height_in_map_units_minus1.checked_add(1)?.checked_mul(2 - frame_mbs_only_flag)?.checked_mul(16)?;

    let chroma_array_type = if separate_colour_plane_flag == 0 { chroma_format_idc } else { 0 };
    let (crop_unit_x, crop_unit_y) = match chroma_array_type {
        0 | 3 => (1, 2 - frame_mbs_only_flag),
        1 => (2, 2 * (2 - frame_mbs_only_flag)),
        2 => (2, 2 - frame_mbs_only_flag),
        _ => return None,
    };
    let w = coded_w.checked_sub(crop_left.checked_add(crop_right)?.checked_mul(crop_unit_x)?)?;
    let h = coded_h.checked_sub(crop_top.checked_add(crop_bottom)?.checked_mul(crop_unit_y)?)?;
    (w > 0 && h > 0).then_some((w, h))
}

struct BitWriter {
    bytes: Vec<u8>,
    cur: u8,
    bits: u8,
}

impl BitWriter {
    fn new() -> Self {
        Self { bytes: Vec::new(), cur: 0, bits: 0 }
    }

    fn write_bit(&mut self, bit: u8) {
        self.cur = (self.cur << 1) | (bit & 1);
        self.bits += 1;
        if self.bits == 8 {
            self.bytes.push(self.cur);
            self.cur = 0;
            self.bits = 0;
        }
    }

    fn write_bits(&mut self, val: u32, count: u8) {
        for shift in (0..count).rev() {
            self.write_bit(((val >> shift) & 1) as u8);
        }
    }

    fn write_ue(&mut self, val: u32) {
        let v = u64::from(val) + 1;
        let bits = 64 - v.leading_zeros() as u8;
        for _ in 0..bits.saturating_sub(1) {
            self.write_bit(0);
        }
        for shift in (0..bits).rev() {
            self.write_bit(((v >> shift) & 1) as u8);
        }
    }

    fn finish_rbsp(mut self) -> Vec<u8> {
        self.write_bit(1);
        while self.bits != 0 {
            self.write_bit(0);
        }
        self.bytes
    }
}

fn nal_escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 4);
    let mut zeros = 0u8;
    for &b in rbsp {
        if zeros == 2 && b <= 0x03 {
            out.push(0x03);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros.saturating_add(1).min(2) } else { 0 };
    }
    out
}

/// Build a spec-valid FLV H.264 sequence header (`0x17 0x00 ... avcC`) whose SPS carries the
/// exact coded dimensions `(width, height)`. Used by integration tests simulating real encoders.
#[doc(hidden)]
pub fn minimal_avc_header(width: u32, height: u32) -> Vec<u8> {
    let w = width.max(2) & !1;
    let h = height.max(2) & !1;
    let mb_w = w.div_ceil(16);
    let mb_h = h.div_ceil(16);
    let crop_right = (mb_w * 16 - w) / 2;
    let crop_bottom = (mb_h * 16 - h) / 2;
    let profile = 66u32;
    let compat = 0xC0u32;
    let level = if w > 1920 || h > 1080 { 51 } else { 42 };

    let mut bw = BitWriter::new();
    bw.write_bits(profile, 8);
    bw.write_bits(compat, 8);
    bw.write_bits(level, 8);
    bw.write_ue(0); // seq_parameter_set_id
    bw.write_ue(0); // log2_max_frame_num_minus4
    bw.write_ue(0); // pic_order_cnt_type
    bw.write_ue(0); // log2_max_pic_order_cnt_lsb_minus4
    bw.write_ue(1); // max_num_ref_frames
    bw.write_bit(0); // gaps_in_frame_num_value_allowed_flag
    bw.write_ue(mb_w - 1);
    bw.write_ue(mb_h - 1);
    bw.write_bit(1); // frame_mbs_only_flag
    bw.write_bit(1); // direct_8x8_inference_flag
    let has_crop = crop_right > 0 || crop_bottom > 0;
    bw.write_bit(u8::from(has_crop));
    if has_crop {
        bw.write_ue(0);
        bw.write_ue(crop_right);
        bw.write_ue(0);
        bw.write_ue(crop_bottom);
    }
    bw.write_bit(0); // vui_parameters_present_flag
    let sps_payload = nal_escape(&bw.finish_rbsp());
    let sps_len = 1 + sps_payload.len();

    let mut tag = Vec::with_capacity(18 + sps_len);
    tag.extend_from_slice(&[0x17, 0x00, 0x00, 0x00, 0x00]);
    tag.extend_from_slice(&[1, profile as u8, compat as u8, level as u8, 0xFF, 0xE1]);
    tag.extend_from_slice(&(sps_len as u16).to_be_bytes());
    tag.push(0x67);
    tag.extend_from_slice(&sps_payload);
    // Minimal Baseline PPS (pps_id=0, sps_id=0, RBSP stop bit).
    tag.extend_from_slice(&[1, 0, 4, 0x68, 0xCE, 0x38, 0x80]);
    tag
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_video_tracks_as_obs_writes_them() {
        // A track's header: 0x96 = ex header, keyframe, Multitrack; 0x00 = one track, SequenceStart.
        let head = video_track_tag(2, true, VIDEO_SEQUENCE_START, b"avc1", 0, &[1, 2]);
        assert_eq!(head, [0x96, 0x00, b'a', b'v', b'c', b'1', 2, 1, 2]);
        assert_eq!(video_extra_track(&head), Some((2, true)));
        // A frame with a composition time (3 bytes, big endian) before its data.
        let frame = video_track_tag(1, false, VIDEO_CODED_FRAMES, b"avc1", 66, &[9]);
        assert_eq!(frame, [0xA6, 0x01, b'a', b'v', b'c', b'1', 1, 0, 0, 66, 9]);
        assert_eq!(video_extra_track(&frame), Some((1, false)));
        // CodedFramesX: no composition time.
        assert_eq!(video_track_tag(1, true, VIDEO_CODED_FRAMES_X, b"avc1", 0, &[7]).len(), 8);
        // The main track, classic AVC and other ex packets aren't extra tracks.
        assert_eq!(video_extra_track(&video_track_tag(0, true, VIDEO_CODED_FRAMES_X, b"avc1", 0, &[])), None);
        assert_eq!(video_extra_track(&[0x17, 0x01]), None);
        assert_eq!(video_extra_track(&[0x91, b'h', b'v', b'c', b'1']), None);
        assert_eq!(video_extra_track(&[0x96, 0x00]), None);
    }

    #[test]
    fn extra_audio_tracks_as_obs_sends_them() {
        // 0x95 = ex header + multitrack; one track, SequenceStart (0) or CodedFrames (1); mp4a; track 1.
        assert_eq!(audio_extra_track(b"\x95\x00mp4a\x01\x12\x10"), Some((1, true)));
        assert_eq!(audio_extra_track(b"\x95\x01mp4a\x01\x21"), Some((1, false)));
        // Track 0, ordinary AAC and short packets aren't extra tracks.
        assert_eq!(audio_extra_track(b"\x95\x01mp4a\x00\x21"), None);
        assert_eq!(audio_extra_track(&[0xAF, 0x01, 0x21]), None);
        assert_eq!(audio_extra_track(&[0x95, 0x01]), None);
        // An extra track's header isn't the main audio header.
        assert!(!audio_is_header(b"\x95\x00mp4a\x01\x12\x10"));
    }

    #[test]
    fn avc_keyframes_and_headers() {
        // 0x17 = keyframe + AVC; packet type 0 = header, 1 = NALUs.
        assert!(video_is_keyframe(&[0x17, 0x00]));
        assert!(video_is_header(&[0x17, 0x00, 0, 0, 0]));
        assert!(!video_is_coded_keyframe(&[0x17, 0x00, 0, 0, 0]));
        assert!(video_is_keyframe(&[0x17, 0x01]));
        assert!(!video_is_header(&[0x17, 0x01]));
        assert!(video_is_coded_keyframe(&[0x17, 0x01]));
        // 0x27 = inter frame.
        assert!(!video_is_keyframe(&[0x27, 0x01]));
        assert!(!video_is_header(&[0x27, 0x01]));
    }

    #[test]
    fn enhanced_rtmp_video() {
        // 0x90 = ex header, keyframe, SequenceStart; 0x91 = keyframe, CodedFrames; 0xA1 = inter.
        assert!(video_is_header(&[0x90, b'h', b'v', b'c', b'1']));
        assert!(video_is_keyframe(&[0x90]));
        assert!(video_is_keyframe(&[0x91]));
        assert!(!video_is_header(&[0x91]));
        assert!(!video_is_keyframe(&[0xA1]));
    }

    #[test]
    fn aac_and_enhanced_audio_headers() {
        assert!(audio_is_header(&[0xAF, 0x00, 0x12, 0x10]));
        assert!(!audio_is_header(&[0xAF, 0x01, 0x21]));
        assert!(audio_is_header(&[0x90, b'O', b'p', b'u', b's']));
        assert!(!audio_is_header(&[0x91]));
        // MP3 (format 2) has no header.
        assert!(!audio_is_header(&[0x2F, 0x00]));
    }

    #[test]
    fn codecs_from_headers() {
        assert_eq!(video_codec(&[0x17, 0x00]), Some(VideoCodec::H264));
        assert_eq!(video_codec(&[0x1C, 0x00]), Some(VideoCodec::Hevc));
        assert_eq!(video_codec(b"\x90hvc1"), Some(VideoCodec::Hevc));
        assert_eq!(video_codec(b"\x90av01"), Some(VideoCodec::Av1));
        assert_eq!(video_codec(b"\x90vp09"), Some(VideoCodec::Vp9));
        assert_eq!(video_codec(b"\x90xxxx"), None);
        assert_eq!(video_codec(b"\x90av"), None);
        assert_eq!(video_codec(&[]), None);
        assert_eq!(video_codec(&[0x12, 0x00]), None);
    }

    #[test]
    fn empty_and_short_tags_are_never_keyframes_or_headers() {
        assert!(!video_is_keyframe(&[]));
        assert!(!video_is_header(&[]));
        assert!(!video_is_header(&[0x17]));
        assert!(!audio_is_header(&[]));
        assert!(!audio_is_header(&[0xAF]));
    }

    #[test]
    fn avcc_sps_parses_standard_and_cropped_resolutions() {
        for (w, h) in [(1280, 720), (1920, 1080), (2560, 1440), (3840, 2160), (1080, 1920)] {
            let hdr = minimal_avc_header(w, h);
            assert!(video_is_header(&hdr));
            assert_eq!(video_codec(&hdr), Some(VideoCodec::H264));
            assert_eq!(avcc_sps_dimensions(&hdr), Some((w, h)), "failed for {w}x{h}");
        }
    }

    #[test]
    fn avcc_sps_parses_real_high_profile_emulation_prevention_and_enhanced_rtmp() {
        // Real High-profile (profile_idc = 100) 1920x1080 SPS with 1088 -> 1080 bottom crop and
        // emulation-prevention 0x00 0x00 0x03 inside VUI/SPS payload (x264/VideoToolbox style).
        let high_1080p_sps: &[u8] = &[
            0x67, 0x64, 0x00, 0x2A, 0xAC, 0xD9, 0x40, 0x78, 0x02, 0x27, 0xE5, 0xC0, 0x44, 0x00, 0x00, 0x03, 0x00, 0x04,
            0x00, 0x00, 0x03, 0x00, 0xF0, 0x3C, 0x60, 0xC6, 0x58,
        ];
        let mut classic = vec![0x17, 0x00, 0, 0, 0, 1, 0x64, 0x00, 0x2A, 0xFF, 0xE1];
        classic.extend_from_slice(&(high_1080p_sps.len() as u16).to_be_bytes());
        classic.extend_from_slice(high_1080p_sps);
        classic.extend_from_slice(&[1, 0, 4, 0x68, 0xEE, 0x3C, 0x80]);
        assert_eq!(avcc_sps_dimensions(&classic), Some((1920, 1080)));

        // Same avcC wrapped inside Enhanced RTMP `avc1` SequenceStart (`0x90 b"avc1"`).
        let mut ex = vec![0x90, b'a', b'v', b'c', b'1'];
        ex.extend_from_slice(&classic[5..]);
        assert_eq!(avcc_sps_dimensions(&ex), Some((1920, 1080)));
    }

    #[test]
    fn avcc_sps_ignores_stub_headers_and_corrupt_data() {
        assert_eq!(avcc_sps_dimensions(&[0x17, 0x00, 0, 0, 0, 1, 0x64]), None);
        assert_eq!(avcc_sps_dimensions(&[0x17, 0x01, 0, 0, 0, 0xAA]), None);
        assert_eq!(avcc_sps_dimensions(b"\x90hvc1\x01\x01"), None);
        assert_eq!(avcc_sps_dimensions(&[]), None);
    }
}
