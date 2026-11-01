//! Matroska (MKV) written as packets come, the format OBS users pick most for recordings: the
//! file plays up to its last cluster even when the computer stops mid-recording. The segment's
//! size is left open (live Matroska); each cluster starts on a video keyframe (or every few
//! seconds of sound alone) and is written whole, with its size, once the next one begins.
//! Times are milliseconds. H.264 and HEVC go in as the encoder's length-prefixed NAL units
//! (V_MPEG4/ISO/AVC, V_MPEGH/ISO/HEVC) with its header as CodecPrivate; AAC as raw frames.

use std::io::{self, Write};

use crate::audio::AudioPacket;
use crate::mp4::{AudioTrack, VideoTrack};
use crate::video::{VideoCodec, VideoPacket};

const VIDEO: u8 = 1;
const AUDIO: u8 = 2;
/// A cluster's blocks sit at most this far (ms) from its start (their offset is 16 bits).
const CLUSTER_MS: i64 = 30_000;
/// Sound alone starts a new cluster this often (ms).
const AUDIO_ONLY_CLUSTER_MS: i64 = 5_000;

/// An element's size in Matroska's variable-length form (8 bytes when it's big, else fewest).
fn size(n: u64) -> Vec<u8> {
    for len in 1..=8u32 {
        // All ones is "unknown", so each length holds one less than its full range.
        let max = (1u64 << (7 * len)) - 1;
        if n < max {
            let v = n | (1u64 << (7 * len));
            return v.to_be_bytes()[(8 - len as usize)..].to_vec();
        }
    }
    let mut v = vec![0x01];
    v.extend_from_slice(&n.to_be_bytes()[1..]);
    v
}

fn element(id: u32, body: &[u8]) -> Vec<u8> {
    let id = id.to_be_bytes();
    let skip = id.iter().take_while(|b| **b == 0).count();
    let mut v = id[skip..].to_vec();
    v.extend_from_slice(&size(body.len() as u64));
    v.extend_from_slice(body);
    v
}

fn uint(id: u32, n: u64) -> Vec<u8> {
    let b = n.to_be_bytes();
    let skip = b.iter().take_while(|x| **x == 0).count().min(7);
    element(id, &b[skip..])
}

fn float(id: u32, f: f64) -> Vec<u8> {
    element(id, &f.to_be_bytes())
}

fn string(id: u32, s: &str) -> Vec<u8> {
    element(id, s.as_bytes())
}

/// Writes an MKV to `out` as packets come.
#[derive(Debug)]
pub struct MkvWriter<W: Write> {
    out: W,
    has_video: bool,
    /// The first time (µs) in the file: it starts at 0 from there.
    start_us: Option<i64>,
    /// The cluster being gathered: its time (ms) and its blocks.
    cluster: Option<(i64, Vec<u8>)>,
    written: u64,
    /// How many sound tracks.
    tracks: usize,
}

impl<W: Write> MkvWriter<W> {
    /// Writes the header at once: the file is valid (empty) from here.
    pub fn new(out: W, video: Option<VideoTrack>, audio: Option<AudioTrack>) -> io::Result<Self> {
        Self::with_tracks(out, video, audio.into_iter().collect())
    }

    /// With several sound tracks (OBS records up to six): `audio_on` fills each; the first plays
    /// by default.
    pub fn with_tracks(mut out: W, video: Option<VideoTrack>, audio: Vec<AudioTrack>) -> io::Result<Self> {
        if video.is_none() && audio.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "a recording needs video or sound"));
        }
        let ebml = element(
            0x1A45_DFA3,
            &[
                uint(0x4286, 1),
                uint(0x42F7, 1),
                uint(0x42F2, 4),
                uint(0x42F3, 8),
                string(0x4282, "matroska"),
                uint(0x4287, 4),
                uint(0x4285, 2),
            ]
            .concat(),
        );
        let app = format!("Ferrocast {}", env!("CARGO_PKG_VERSION"));
        let info =
            element(0x1549_A966, &[uint(0x2AD7B1, 1_000_000), string(0x4D80, &app), string(0x5741, &app)].concat());
        let mut tracks = Vec::new();
        if let Some(v) = &video {
            let codec = if v.codec == VideoCodec::Hevc { "V_MPEGH/ISO/HEVC" } else { "V_MPEG4/ISO/AVC" };
            tracks.extend(element(
                0xAE,
                &[
                    uint(0xD7, u64::from(VIDEO)),
                    uint(0x73C5, u64::from(VIDEO)),
                    uint(0x83, 1),
                    uint(0x9C, 0),
                    string(0x86, codec),
                    element(0x63A2, &v.config),
                    element(0xE0, &[uint(0xB0, u64::from(v.width)), uint(0xBA, u64::from(v.height))].concat()),
                ]
                .concat(),
            ));
        }
        for (i, a) in audio.iter().enumerate() {
            let n = u64::from(AUDIO) + i as u64;
            tracks.extend(element(
                0xAE,
                &[
                    uint(0xD7, n),
                    uint(0x73C5, n),
                    uint(0x83, 2),
                    uint(0x88, u64::from(i == 0)),
                    uint(0x9C, 0),
                    string(0x86, "A_AAC"),
                    element(0x63A2, &a.config),
                    element(0xE1, &[float(0xB5, f64::from(a.sample_rate)), uint(0x9F, u64::from(a.channels))].concat()),
                ]
                .concat(),
            ));
        }
        let mut head = ebml;
        // The segment's size stays unknown: the file can end anywhere and still play.
        head.extend_from_slice(&[0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        head.extend(info);
        head.extend(element(0x1654_AE6B, &tracks));
        out.write_all(&head)?;
        out.flush()?;
        Ok(Self {
            out,
            has_video: video.is_some(),
            start_us: None,
            cluster: None,
            written: head.len() as u64,
            tracks: audio.len(),
        })
    }

    /// A video frame (shown at its presentation time). A keyframe starts a new cluster.
    pub fn video(&mut self, p: &VideoPacket) -> io::Result<()> {
        if self.start_us.is_none() && !p.key {
            // Nothing decodes before the first keyframe.
            return Ok(());
        }
        let start = *self.start_us.get_or_insert(p.pts_us.min(p.dts_us));
        let ms = (p.pts_us - start) / 1000;
        self.block(VIDEO, ms, p.key, &p.data, p.key)
    }

    /// An AAC frame for the first sound track.
    pub fn audio(&mut self, p: &AudioPacket) -> io::Result<()> {
        self.audio_on(0, p)
    }

    /// An AAC frame for sound track `track` (0 is the first).
    pub fn audio_on(&mut self, track: usize, p: &AudioPacket) -> io::Result<()> {
        if track >= self.tracks {
            return Ok(());
        }
        let start = match self.start_us {
            Some(s) => s,
            // Sound before the first picture waits out (it'd start the file early).
            None if self.has_video => return Ok(()),
            None => *self.start_us.get_or_insert(p.pts_us),
        };
        if p.pts_us < start {
            return Ok(());
        }
        let ms = (p.pts_us - start) / 1000;
        let new = !self.has_video
            && track == 0
            && self.cluster.as_ref().is_none_or(|(at, _)| ms - at >= AUDIO_ONLY_CLUSTER_MS);
        self.block(AUDIO + track as u8, ms, true, &p.data, new)
    }

    fn block(&mut self, track: u8, ms: i64, key: bool, data: &[u8], new_cluster: bool) -> io::Result<()> {
        let far = self.cluster.as_ref().is_some_and(|(at, _)| (ms - at).abs() >= CLUSTER_MS);
        if new_cluster || far || self.cluster.is_none() {
            self.flush_cluster()?;
            self.cluster = Some((ms.max(0), Vec::new()));
        }
        let Some((at, blocks)) = self.cluster.as_mut() else { return Ok(()) };
        let rel = i16::try_from(ms - *at).unwrap_or(if ms < *at { i16::MIN } else { i16::MAX });
        let mut body = vec![0x80 | track];
        body.extend_from_slice(&rel.to_be_bytes());
        body.push(if key { 0x80 } else { 0 });
        body.extend_from_slice(data);
        blocks.extend(element(0xA3, &body));
        Ok(())
    }

    fn flush_cluster(&mut self) -> io::Result<()> {
        if let Some((at, blocks)) = self.cluster.take() {
            let c = element(0x1F43_B675, &[uint(0xE7, at.max(0) as u64), blocks].concat());
            self.out.write_all(&c)?;
            self.out.flush()?;
            self.written += c.len() as u64;
        }
        Ok(())
    }

    /// Bytes written to the output so far (the cluster being gathered isn't).
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Writes out the last cluster and hands the output back.
    pub fn finish(mut self) -> io::Result<W> {
        self.flush_cluster()?;
        self.out.flush()?;
        Ok(self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_take_the_fewest_bytes() {
        assert_eq!(size(0), [0x80]);
        assert_eq!(size(126), [0xFE]);
        // 127 is "unknown" in one byte, so it takes two.
        assert_eq!(size(127), [0x40, 0x7F]);
        assert_eq!(size(300), [0x41, 0x2C]);
        assert_eq!(element(0x1A45_DFA3, &[1, 2])[..5], [0x1A, 0x45, 0xDF, 0xA3, 0x82]);
        assert_eq!(uint(0xD7, 1), [0xD7, 0x81, 0x01]);
        assert_eq!(uint(0xE7, 0), [0xE7, 0x81, 0x00]);
    }
}
