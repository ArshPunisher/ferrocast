//! Matroska (MKV) written as packets come, the format OBS users pick most for recordings: the
//! file plays up to its last cluster even when the computer stops mid-recording. The segment's
//! size is left open (live Matroska); each cluster starts on a video keyframe (or every few
//! seconds of sound alone) and is written whole, with its size, once the next one begins.
//! Times are milliseconds. H.264 and HEVC go in as the encoder's length-prefixed NAL units
//! (V_MPEG4/ISO/AVC, V_MPEGH/ISO/HEVC) with its header as CodecPrivate; AAC as raw frames.

use std::io::{self, Seek, SeekFrom, Write};

use crate::audio::AudioPacket;
use crate::mp4::{AudioTrack, VideoTrack};
use crate::video::{VideoCodec, VideoPacket};

const VIDEO: u8 = 1;
const AUDIO: u8 = 2;
/// A cluster's blocks sit at most this far (ms) from its start (their offset is 16 bits).
const CLUSTER_MS: i64 = 30_000;
/// Sound alone starts a new cluster this often (ms).
const AUDIO_ONLY_CLUSTER_MS: i64 = 5_000;
/// Room inside Info for the duration: its element (11 bytes) and a Void for what is left (3).
const DURATION_ROOM: usize = 14;
/// What a finish that failed hands back (shared with MP4's).
pub use crate::mp4::FinishFailed;

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
    /// Where the room for chapter marks (a Void element) starts.
    room_at: u64,
    /// Chapter marks: where (µs from the file's start) and their names.
    chapters: Vec<(i64, String)>,
    /// The latest time written (µs from the file's start): where a mark goes.
    last_us: i64,
    /// Where the segment's data starts (what positions in the index count from), and its size field.
    segment_at: u64,
    /// Where Info and Tracks start, and the room inside Info kept for the duration.
    info_at: u64,
    tracks_at: u64,
    duration_room: u64,
    /// Each cluster written: its time (ms) and where it starts in the file (for the index).
    cues: Vec<(i64, u64)>,
}

/// A Void element exactly `len` bytes long (at least 2), or None.
fn void(len: usize) -> Option<Vec<u8>> {
    let mut v = vec![0xEC];
    if (2..129).contains(&len) {
        v.push(0x80 | (len - 2) as u8);
    } else if len >= 9 {
        v.push(0x01);
        v.extend_from_slice(&((len - 9) as u64).to_be_bytes()[1..]);
    } else {
        return None;
    }
    v.resize(len, 0);
    Some(v)
}

/// Matroska's Chapters: one edition, a mark per chapter (its start in ns, its name).
fn chapters_element(chapters: &[(i64, String)]) -> Vec<u8> {
    let atoms: Vec<u8> = chapters
        .iter()
        .enumerate()
        .flat_map(|(i, (us, name))| {
            element(
                0xB6,
                &[
                    uint(0x73C4, i as u64 + 1),
                    uint(0x91, us.max(&0).unsigned_abs() * 1000),
                    element(0x80, &[string(0x85, name), string(0x437C, "eng")].concat()),
                ]
                .concat(),
            )
        })
        .collect();
    element(0x1043_A770, &element(0x45B9, &atoms))
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
        // A Void inside Info, where the duration goes when the file is finished.
        let info = element(
            0x1549_A966,
            &[
                uint(0x2AD7B1, 1_000_000),
                string(0x4D80, &app),
                string(0x5741, &app),
                void(DURATION_ROOM).unwrap_or_default(),
            ]
            .concat(),
        );
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
        let segment_at = head.len() as u64;
        let info_at = segment_at;
        let duration_room = info_at + info.len() as u64 - DURATION_ROOM as u64;
        head.extend(info);
        let tracks_at = head.len() as u64;
        head.extend(element(0x1654_AE6B, &tracks));
        let room_at = head.len() as u64;
        head.extend(void(crate::mp4::ROOM).unwrap_or_default());
        out.write_all(&head)?;
        out.flush()?;
        Ok(Self {
            out,
            has_video: video.is_some(),
            start_us: None,
            cluster: None,
            written: head.len() as u64,
            tracks: audio.len(),
            room_at,
            chapters: Vec::new(),
            last_us: 0,
            segment_at,
            info_at,
            tracks_at,
            duration_room,
            cues: Vec::new(),
        })
    }

    /// A chapter mark here (OBS's "Add Chapter Marker"), named, or "Chapter n" when not.
    /// False when the file has `MAX_CHAPTERS` already.
    pub fn chapter(&mut self, name: &str) -> bool {
        if self.chapters.len() >= crate::mp4::MAX_CHAPTERS {
            return false;
        }
        let name = crate::mp4::chapter_name(name, self.chapters.len() + 1, 255);
        self.chapters.push((self.last_us, name));
        true
    }

    /// A video frame (shown at its presentation time). A keyframe starts a new cluster.
    pub fn video(&mut self, p: &VideoPacket) -> io::Result<()> {
        if self.start_us.is_none() && !p.key {
            // Nothing decodes before the first keyframe.
            return Ok(());
        }
        let start = *self.start_us.get_or_insert(p.pts_us.min(p.dts_us));
        self.last_us = self.last_us.max(p.pts_us - start);
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
        self.last_us = self.last_us.max(p.pts_us - start);
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
            // Where it starts, for the index a finished file has.
            self.cues.push((at.max(0), self.written));
            self.written += c.len() as u64;
        }
        Ok(())
    }

    /// Bytes written to the output so far (the cluster being gathered isn't).
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Gives the output up without writing the cluster being gathered (a disk that can't take
    /// it): the output, and where the last whole cluster ends (see `Mp4Writer::abandon`).
    pub fn abandon(self) -> (W, u64) {
        (self.out, self.written)
    }

    /// Writes out the last cluster and hands the output back.
    pub fn finish(mut self) -> io::Result<W> {
        self.flush_cluster()?;
        self.out.flush()?;
        Ok(self.out)
    }
}

impl<W: Write + Seek> MkvWriter<W> {
    /// As `finalize`, with the error flattened (the writer is dropped: nothing is cut back).
    pub fn finish_with_chapters(self) -> io::Result<W> {
        self.finalize().map_err(|f| f.error)
    }

    /// Writes out the last cluster and makes the file a finished Matroska: the duration in Info,
    /// an index of the clusters (Cues) at the end, a SeekHead that says where Info, Tracks, the
    /// chapters and the index are, the chapters, and the segment's size. A player then shows the
    /// length and seeks without reading the file through. If any step fails the header is put back
    /// as it was (the file plays as the open-ended one it was), and the failure hands back the
    /// output and where the last whole cluster ends, to cut back to.
    pub fn finalize(mut self) -> Result<W, FinishFailed<W>> {
        macro_rules! fail {
            ($self:ident, $e:expr) => {{
                let whole = $self.written;
                return Err(FinishFailed { error: $e, out: $self.out, whole });
            }};
        }
        if let Err(e) = self.flush_cluster().and_then(|()| self.out.flush()) {
            fail!(self, e);
        }
        if self.cues.is_empty() {
            return Ok(self.out);
        }
        let track = if self.has_video { VIDEO } else { AUDIO };
        let points: Vec<u8> = self
            .cues
            .iter()
            .flat_map(|(ms, at)| {
                element(
                    0xBB,
                    &[
                        uint(0xB3, *ms as u64),
                        element(0xB7, &[uint(0xF7, u64::from(track)), uint(0xF1, at - self.segment_at)].concat()),
                    ]
                    .concat(),
                )
            })
            .collect();
        let cues = element(0x1C53_BB6B, &points);
        let end = self.written;
        let chapters = self.chapters.clone();
        // The room after Tracks: the SeekHead (a fixed size for the same entries), then the
        // chapters that fit, then a Void for what is left.
        let seek = |positions: [u64; 4], chapters: bool| -> Vec<u8> {
            let entry = |id: &[u8], at: u64| {
                element(0x4DBB, &[element(0x53AB, id), element(0x53AC, &at.to_be_bytes())].concat())
            };
            let mut list =
                [entry(&[0x15, 0x49, 0xA9, 0x66], positions[0]), entry(&[0x16, 0x54, 0xAE, 0x6B], positions[1])]
                    .concat();
            if chapters {
                list.extend(entry(&[0x10, 0x43, 0xA7, 0x70], positions[2]));
            }
            list.extend(entry(&[0x1C, 0x53, 0xBB, 0x6B], positions[3]));
            element(0x114D_9B74, &list)
        };
        let mut room = None;
        for n in (0..=chapters.len()).rev() {
            let c = if n == 0 { Vec::new() } else { chapters_element(&chapters[..n]) };
            let seek_len = seek([0; 4], n > 0).len();
            let Some(rest) = crate::mp4::ROOM.checked_sub(seek_len + c.len()) else { continue };
            let rest = if rest == 0 { Some(Vec::new()) } else { void(rest) };
            if let Some(rest) = rest {
                let chapters_at = self.room_at + seek_len as u64;
                let rel = |at: u64| at - self.segment_at;
                let head = seek([rel(self.info_at), rel(self.tracks_at), rel(chapters_at), rel(end)], n > 0);
                room = Some([head, c, rest].concat());
                break;
            }
        }
        let Some(room) = room else {
            fail!(self, io::Error::other("no room for the index"));
        };
        let original_room = void(crate::mp4::ROOM).unwrap_or_default();
        let duration_ms = (self.last_us.max(0) as f64) / 1000.0;
        let duration = [float(0x4489, duration_ms), void(DURATION_ROOM - 11).unwrap_or_default()].concat();
        let void_duration = void(DURATION_ROOM).unwrap_or_default();
        // 1. The index at the end. Failing here leaves the open-ended file with a piece of index
        //    past its last cluster, which the caller cuts off.
        let wrote =
            self.out.seek(SeekFrom::Start(end)).and_then(|_| self.out.write_all(&cues)).and_then(|()| self.out.flush());
        if let Err(e) = wrote {
            fail!(self, e);
        }
        // 2. The header: duration, SeekHead and chapters, then the segment's size. Put back if one fails.
        let size = end + cues.len() as u64 - self.segment_at;
        let patch = (|| -> io::Result<()> {
            self.out.seek(SeekFrom::Start(self.duration_room))?;
            self.out.write_all(&duration)?;
            self.out.seek(SeekFrom::Start(self.room_at))?;
            self.out.write_all(&room)?;
            // The segment's size, in 8 bytes (the length marker, then 7).
            self.out.seek(SeekFrom::Start(self.segment_at - 8))?;
            let mut field = vec![0x01];
            field.extend_from_slice(&size.to_be_bytes()[1..]);
            self.out.write_all(&field)?;
            self.out.seek(SeekFrom::End(0))?;
            self.out.flush()
        })();
        if let Err(e) = patch {
            let _ = (|| -> io::Result<()> {
                self.out.seek(SeekFrom::Start(self.duration_room))?;
                self.out.write_all(&void_duration)?;
                self.out.seek(SeekFrom::Start(self.room_at))?;
                self.out.write_all(&original_room)?;
                self.out.seek(SeekFrom::Start(self.segment_at - 8))?;
                self.out.write_all(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])?;
                self.out.flush()
            })();
            fail!(self, e);
        }
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
