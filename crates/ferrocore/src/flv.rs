//! FLV tag bodies, as RTMP carries them (and as Go live's hub passes them on): H.264 and AAC the
//! classic way, HEVC as Enhanced RTMP (veovera.org's spec, which Twitch and YouTube take).

use crate::video::{VideoCodec, VideoPacket};

/// Classic FLV: codec id 7 (AVC); packet types 0 (sequence header) and 1 (NAL units).
const AVC: u8 = 7;
/// Enhanced RTMP: the "ex header" bit, packet types SequenceStart (0), CodedFrames (1) and
/// CodedFramesX (3, no composition time), and HEVC's FourCC.
const EX_HEADER: u8 = 0x80;
const HVC1: [u8; 4] = *b"hvc1";
/// Frame types: keyframe (1), inter frame (2).
const KEY: u8 = 1;
const INTER: u8 = 2;

/// The codec header tag: the decoder configuration record (avcC or hvcC).
pub fn video_header(codec: VideoCodec, record: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(record.len() + 8);
    match codec {
        VideoCodec::H264 => out.extend_from_slice(&[KEY << 4 | AVC, 0, 0, 0, 0]),
        VideoCodec::Hevc => {
            out.push(EX_HEADER | KEY << 4);
            out.extend_from_slice(&HVC1);
        }
    }
    out.extend_from_slice(record);
    out
}

/// Enhanced RTMP's Multitrack video packet type (one track a packet, `trackId` after the FourCC),
/// as OBS's `flv_packet_ex` writes Twitch Enhanced Broadcasting's extra qualities. H.264 only.
const MULTITRACK: u8 = 6;
const AVC1: [u8; 4] = *b"avc1";

/// An extra video track's codec header (its AVCDecoderConfigurationRecord), `track` 1 or more.
pub fn video_track_header(track: u8, record: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(record.len() + 7);
    out.push(EX_HEADER | KEY << 4 | MULTITRACK);
    out.push(0); // one track, SequenceStart
    out.extend_from_slice(&AVC1);
    out.push(track);
    out.extend_from_slice(record);
    out
}

/// An extra video track's frame and its time in ms, as `video_tag` times the main track's:
/// CodedFrames with the composition time, or CodedFramesX when there's none.
pub fn video_track_tag(track: u8, p: &VideoPacket, start_us: i64) -> (u32, Vec<u8>) {
    let ms = |us: i64| (us - start_us).max(0) / 1000;
    let dts = ms(p.dts_us);
    let cts = (ms(p.pts_us) - dts).clamp(-(1 << 23), (1 << 23) - 1) as i32;
    let kind = if p.key { KEY } else { INTER };
    let mut out = Vec::with_capacity(p.data.len() + 10);
    out.push(EX_HEADER | kind << 4 | MULTITRACK);
    out.push(if cts == 0 { 3 } else { 1 });
    out.extend_from_slice(&AVC1);
    out.push(track);
    if cts != 0 {
        out.extend_from_slice(&cts.to_be_bytes()[1..]);
    }
    out.extend_from_slice(&p.data);
    (dts as u32, out)
}

/// A frame's tag, and its timestamp in milliseconds (the decode time; the composition offset is
/// inside). Times before `start_us` are clamped to 0.
pub fn video_tag(codec: VideoCodec, p: &VideoPacket, start_us: i64) -> (u32, Vec<u8>) {
    let ms = |us: i64| (us - start_us).max(0) / 1000;
    let dts = ms(p.dts_us);
    let cts = (ms(p.pts_us) - dts).clamp(-(1 << 23), (1 << 23) - 1) as i32;
    let kind = if p.key { KEY } else { INTER };
    let mut out = Vec::with_capacity(p.data.len() + 8);
    match codec {
        VideoCodec::H264 => {
            out.push(kind << 4 | AVC);
            out.push(1);
            out.extend_from_slice(&cts.to_be_bytes()[1..]);
        }
        VideoCodec::Hevc if cts == 0 => {
            out.push(EX_HEADER | kind << 4 | 3);
            out.extend_from_slice(&HVC1);
        }
        VideoCodec::Hevc => {
            out.push(EX_HEADER | kind << 4 | 1);
            out.extend_from_slice(&HVC1);
            out.extend_from_slice(&cts.to_be_bytes()[1..]);
        }
    }
    out.extend_from_slice(&p.data);
    (u32::try_from(dts).unwrap_or(u32::MAX), out)
}

/// AAC's header tag: its AudioSpecificConfig. (0xAF: AAC, 44 kHz/16-bit/stereo flags, as FLV
/// always writes for AAC; the real rate is in the config.)
pub fn audio_header(config: &[u8]) -> Vec<u8> {
    let mut out = vec![0xAF, 0];
    out.extend_from_slice(config);
    out
}

/// One AAC frame's tag.
pub fn audio_tag(frame: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(frame.len() + 2);
    out.extend_from_slice(&[0xAF, 1]);
    out.extend_from_slice(frame);
    out
}

/// An extra AAC track's tag (Enhanced RTMP multitrack audio, as OBS sends Twitch's VOD
/// track): `header` for its AudioSpecificConfig, else one frame; `track` is 1 or more.
pub fn audio_track_tag(track: u8, header: bool, data: &[u8]) -> Vec<u8> {
    // ExHeader (9) with MultiTrack (5); one track, sequence start (0) or coded frames (1).
    let mut out = vec![0x95, if header { 0 } else { 1 }, b'm', b'p', b'4', b'a', track];
    out.extend_from_slice(data);
    out
}

/// A whole FLV file: its header, then each tag with its size after it. For recordings and tests;
/// streams send the tag bodies over RTMP.
#[derive(Debug, Default)]
pub struct FlvFile {
    pub bytes: Vec<u8>,
}

impl FlvFile {
    pub fn new(audio: bool, video: bool) -> Self {
        let flags = (u8::from(audio) << 2) | u8::from(video);
        let mut bytes = b"FLV\x01".to_vec();
        bytes.extend_from_slice(&[flags, 0, 0, 0, 9, 0, 0, 0, 0]);
        Self { bytes }
    }

    /// Kind 8 audio, 9 video.
    pub fn tag(&mut self, kind: u8, ms: u32, body: &[u8]) {
        let size = body.len() as u32;
        self.bytes.push(kind);
        self.bytes.extend_from_slice(&size.to_be_bytes()[1..]);
        self.bytes.extend_from_slice(&ms.to_be_bytes()[1..]);
        self.bytes.push((ms >> 24) as u8);
        self.bytes.extend_from_slice(&[0, 0, 0]);
        self.bytes.extend_from_slice(body);
        self.bytes.extend_from_slice(&(size + 11).to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(pts: i64, dts: i64, key: bool) -> VideoPacket {
        VideoPacket { pts_us: pts, dts_us: dts, key, data: vec![0, 0, 0, 1, 0x65] }
    }

    #[test]
    fn h264_tags_are_classic_flv() {
        assert_eq!(video_header(VideoCodec::H264, &[1, 2])[..5], [0x17, 0, 0, 0, 0]);
        let (ms, t) = video_tag(VideoCodec::H264, &packet(1_040_000, 1_000_000, true), 0);
        assert_eq!(ms, 1000);
        assert_eq!(t[..5], [0x17, 1, 0, 0, 40]);
        let (_, t) = video_tag(VideoCodec::H264, &packet(0, 0, false), 0);
        assert_eq!(t[0], 0x27);
    }

    #[test]
    fn hevc_tags_are_enhanced_rtmp() {
        assert_eq!(video_header(VideoCodec::Hevc, &[1])[..5], [0x90, b'h', b'v', b'c', b'1']);
        let (_, t) = video_tag(VideoCodec::Hevc, &packet(5000, 5000, false), 0);
        assert_eq!(t[..5], [0xA3, b'h', b'v', b'c', b'1']);
        let (_, t) = video_tag(VideoCodec::Hevc, &packet(40_000, 0, true), 0);
        assert_eq!(t[..8], [0x91, b'h', b'v', b'c', b'1', 0, 0, 40]);
    }

    #[test]
    fn times_start_at_zero() {
        let (ms, _) = video_tag(VideoCodec::H264, &packet(500, 500, true), 1000);
        assert_eq!(ms, 0);
    }

    #[test]
    fn a_file_has_its_header_and_sizes() {
        let mut f = FlvFile::new(true, true);
        f.tag(9, 0x0102_0304, &[7; 3]);
        assert_eq!(&f.bytes[..5], b"FLV\x01\x05");
        // Tag: kind, size 3, time 0x020304 + extended 0x01, stream 0, body, previous size 14.
        assert_eq!(&f.bytes[13..], &[9, 0, 0, 3, 2, 3, 4, 1, 0, 0, 0, 7, 7, 7, 0, 0, 0, 14]);
    }
}
