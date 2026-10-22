//! Just enough of FLV's audio and video tags to forward them untouched: which video tag starts a
//! picture a player can decode on its own (a keyframe), and which tags are the codec headers a
//! player needs before anything else. A destination that joins mid-stream gets the headers again
//! and starts at the next keyframe. Covers classic AVC and AAC, and Enhanced RTMP (HEVC, AV1,
//! VP9 video and its audio, as OBS 30+ sends them).

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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn empty_and_short_tags_are_never_keyframes_or_headers() {
        assert!(!video_is_keyframe(&[]));
        assert!(!video_is_header(&[]));
        assert!(!video_is_header(&[0x17]));
        assert!(!audio_is_header(&[]));
        assert!(!audio_is_header(&[0xAF]));
    }
}
