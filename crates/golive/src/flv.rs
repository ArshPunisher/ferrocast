//! Just enough of FLV's audio and video tags to forward them untouched: which video tag starts a
//! picture a player can decode on its own (a keyframe), and which tags are the codec headers a
//! player needs before anything else. A destination that joins mid-stream gets the headers again
//! and starts at the next keyframe. Covers classic AVC and AAC, and Enhanced RTMP (HEVC, AV1,
//! VP9 video and its audio, as OBS 30+ sends them).

use serde::Serialize;

/// The video codec, as its header names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
        assert!(video_is_keyframe(&[0x17, 0x01]));
        assert!(!video_is_header(&[0x17, 0x01]));
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
}
