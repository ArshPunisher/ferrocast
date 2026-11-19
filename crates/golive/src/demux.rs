//! MPEG-TS back into FLV tags (the other way from `ts`), for streams that arrive as TS (SRT,
//! SRTLA) and leave by RTMP: H.264 from Annex B into length-prefixed frames with its
//! AVCDecoderConfigurationRecord as the header, AAC from ADTS into raw frames with its
//! AudioSpecificConfig. One program; times in ms from the first one seen.

use bytes::Bytes;

use crate::ts::PACKET;

/// What a tag is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagKind {
    Video,
    Audio,
}

/// One FLV tag's body, ready for the hub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub kind: TagKind,
    /// Decode time, ms.
    pub ms: u32,
    pub body: Bytes,
}

/// Why a stream can't be carried.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DemuxError {
    #[error("the video isn't H.264 (stream type {0:#04x}): send H.264 to carry it this way")]
    Video(u8),
}

const H264: u8 = 0x1B;
const ADTS: u8 = 0x0F;
/// A PES held for more than this is dropped (a broken stream mustn't grow without end).
const MAX_PES: usize = 8 << 20;

#[derive(Debug, Default)]
struct Pes {
    data: Vec<u8>,
}

/// Reads TS as it comes; each call hands back the tags it finished.
#[derive(Debug, Default)]
pub struct TsDemuxer {
    pmt_pid: Option<u16>,
    video_pid: Option<u16>,
    audio_pid: Option<u16>,
    video: Pes,
    audio: Pes,
    /// Bytes of a packet cut across calls.
    carry: Vec<u8>,
    /// The first time (90 kHz, unwrapped) and the last raw one per stream (for wrapping).
    base: Option<i64>,
    last_raw: [Option<i64>; 2],
    wraps: [i64; 2],
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    avc_sent: bool,
    asc: Option<[u8; 2]>,
    /// Said once: the video can't be carried.
    refused: Option<DemuxError>,
}

impl TsDemuxer {
    pub fn new() -> Self {
        Self::default()
    }

    /// The video type this stream has that can't be carried, once seen.
    pub fn refused(&self) -> Option<&DemuxError> {
        self.refused.as_ref()
    }

    /// Feeds bytes (any cut: whole packets are read, the rest kept) → finished tags.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Tag> {
        let mut out = Vec::new();
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(bytes);
        let mut at = 0;
        while at + PACKET <= buf.len() {
            if buf[at] != 0x47 {
                // Lost step: find the next sync byte.
                at += 1;
                continue;
            }
            self.packet(&buf[at..at + PACKET], &mut out);
            at += PACKET;
        }
        self.carry = buf[at..].to_vec();
        out
    }

    fn packet(&mut self, p: &[u8], out: &mut Vec<Tag>) {
        let start = p[1] & 0x40 != 0;
        let pid = (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2]);
        let afc = (p[3] >> 4) & 3;
        let mut at = 4;
        if afc & 2 != 0 {
            at += 1 + usize::from(p[4]);
        }
        if afc & 1 == 0 || at >= PACKET {
            return;
        }
        let payload = &p[at..];
        if pid == 0 {
            if start {
                self.pat(payload);
            }
        } else if Some(pid) == self.pmt_pid {
            if start {
                self.pmt(payload);
            }
        } else if Some(pid) == self.video_pid {
            if start && !self.video.data.is_empty() {
                let pes = std::mem::take(&mut self.video.data);
                self.video_pes(&pes, out);
            }
            if start || !self.video.data.is_empty() {
                self.video.data.extend_from_slice(payload);
            }
            if self.video.data.len() > MAX_PES {
                self.video.data.clear();
            }
        } else if Some(pid) == self.audio_pid {
            if start && !self.audio.data.is_empty() {
                let pes = std::mem::take(&mut self.audio.data);
                self.audio_pes(&pes, out);
            }
            if start || !self.audio.data.is_empty() {
                self.audio.data.extend_from_slice(payload);
            }
            if self.audio.data.len() > MAX_PES {
                self.audio.data.clear();
            }
        }
    }

    /// The section after the pointer field, its length checked.
    fn section(payload: &[u8]) -> Option<&[u8]> {
        let ptr = usize::from(*payload.first()?);
        let s = payload.get(1 + ptr..)?;
        let len = (usize::from(s.get(1)? & 0x0F) << 8) | usize::from(*s.get(2)?);
        s.get(..3 + len)
    }

    fn pat(&mut self, payload: &[u8]) {
        let Some(s) = Self::section(payload) else { return };
        // Programs from byte 8 to the CRC; the first that isn't the network's.
        let Some(body) = s.get(8..s.len().saturating_sub(4)) else { return };
        for e in body.as_chunks::<4>().0 {
            let program = u16::from_be_bytes([e[0], e[1]]);
            if program != 0 {
                self.pmt_pid = Some((u16::from(e[2] & 0x1F) << 8) | u16::from(e[3]));
                return;
            }
        }
    }

    fn pmt(&mut self, payload: &[u8]) {
        let Some(s) = Self::section(payload) else { return };
        let Some(&hi) = s.get(10) else { return };
        let info = (usize::from(hi & 0x0F) << 8) | usize::from(*s.get(11).unwrap_or(&0));
        let mut at = 12 + info;
        let end = s.len().saturating_sub(4);
        while at + 5 <= end {
            let kind = s[at];
            let pid = (u16::from(s[at + 1] & 0x1F) << 8) | u16::from(s[at + 2]);
            let es_info = (usize::from(s[at + 3] & 0x0F) << 8) | usize::from(s[at + 4]);
            match kind {
                H264 => self.video_pid = Some(pid),
                ADTS => self.audio_pid = Some(pid),
                0x24 | 0x02 | 0x10 if self.video_pid.is_none() => {
                    self.refused.get_or_insert(DemuxError::Video(kind));
                }
                _ => {}
            }
            at += 5 + es_info;
        }
    }

    /// A PES's PTS and DTS (90 kHz, raw) and its payload.
    fn pes(data: &[u8]) -> Option<(i64, i64, &[u8])> {
        if data.get(..3)? != [0, 0, 1] {
            return None;
        }
        let flags = *data.get(7)?;
        let hlen = usize::from(*data.get(8)?);
        let time = |b: &[u8]| -> i64 {
            (i64::from(b[0] >> 1 & 7) << 30)
                | (i64::from(b[1]) << 22)
                | (i64::from(b[2] >> 1) << 15)
                | (i64::from(b[3]) << 7)
                | i64::from(b[4] >> 1)
        };
        let pts = if flags & 0x80 != 0 { time(data.get(9..14)?) } else { return None };
        let dts = if flags & 0x40 != 0 { time(data.get(14..19)?) } else { pts };
        Some((pts, dts, data.get(9 + hlen..)?))
    }

    /// A 33-bit time unwrapped, counted from the first one: ms. `which`: 0 video, 1 audio.
    fn ms(&mut self, which: usize, raw: i64) -> u32 {
        const WRAP: i64 = 1 << 33;
        if let Some(last) = self.last_raw[which] {
            if last - raw > WRAP / 2 {
                self.wraps[which] += 1;
            } else if raw - last > WRAP / 2 {
                self.wraps[which] -= 1;
            }
        }
        self.last_raw[which] = Some(raw);
        let t = raw + self.wraps[which] * WRAP;
        let base = *self.base.get_or_insert(t);
        ((t - base).max(0) / 90) as u32
    }

    fn video_pes(&mut self, data: &[u8], out: &mut Vec<Tag>) {
        let Some((pts, dts, es)) = Self::pes(data) else { return };
        let mut key = false;
        let mut frame = Vec::with_capacity(es.len() + 16);
        let (mut sps, mut pps) = (None, None);
        for nal in nals(es) {
            let Some(&h) = nal.first() else { continue };
            match h & 0x1F {
                7 => sps = Some(nal.to_vec()),
                8 => pps = Some(nal.to_vec()),
                9 => {}
                t => {
                    key |= t == 5;
                    frame.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    frame.extend_from_slice(nal);
                }
            }
        }
        let ms = self.ms(0, dts);
        if sps.is_some() && sps != self.sps || pps.is_some() && pps != self.pps {
            if sps.is_some() {
                self.sps = sps;
            }
            if pps.is_some() {
                self.pps = pps;
            }
            self.avc_sent = false;
        }
        if !self.avc_sent
            && let (Some(s), Some(p)) = (&self.sps, &self.pps)
            && s.len() >= 4
        {
            let mut b = vec![0x17, 0, 0, 0, 0, 1, s[1], s[2], s[3], 0xFF, 0xE1];
            b.extend_from_slice(&(s.len() as u16).to_be_bytes());
            b.extend_from_slice(s);
            b.push(1);
            b.extend_from_slice(&(p.len() as u16).to_be_bytes());
            b.extend_from_slice(p);
            out.push(Tag { kind: TagKind::Video, ms, body: b.into() });
            self.avc_sent = true;
        }
        if frame.is_empty() || !self.avc_sent {
            return;
        }
        let cts = ((pts - dts).max(0) / 90) as u32;
        let mut b = vec![if key { 0x17 } else { 0x27 }, 1, (cts >> 16) as u8, (cts >> 8) as u8, cts as u8];
        b.extend_from_slice(&frame);
        out.push(Tag { kind: TagKind::Video, ms, body: b.into() });
    }

    fn audio_pes(&mut self, data: &[u8], out: &mut Vec<Tag>) {
        let Some((pts, _, mut es)) = Self::pes(data) else { return };
        let start = self.ms(1, pts);
        let mut n = 0u32;
        while es.len() >= 7 && es[0] == 0xFF && es[1] & 0xF0 == 0xF0 {
            let protect_absent = es[1] & 1 == 1;
            let profile = es[2] >> 6;
            let rate = (es[2] >> 2) & 0x0F;
            let channels = ((es[2] & 1) << 2) | (es[3] >> 6);
            let len = (usize::from(es[3] & 3) << 11) | (usize::from(es[4]) << 3) | usize::from(es[5] >> 5);
            let head = if protect_absent { 7 } else { 9 };
            let Some(raw) = es.get(head..len) else { break };
            let hz = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350]
                .get(usize::from(rate))
                .copied()
                .unwrap_or(48000);
            let asc = {
                let v = (u16::from(profile + 1) << 11) | (u16::from(rate) << 7) | (u16::from(channels) << 3);
                v.to_be_bytes()
            };
            if self.asc != Some(asc) {
                self.asc = Some(asc);
                out.push(Tag { kind: TagKind::Audio, ms: start, body: vec![0xAF, 0, asc[0], asc[1]].into() });
            }
            let ms = start + n * 1024 * 1000 / hz;
            let mut b = Vec::with_capacity(raw.len() + 2);
            b.extend_from_slice(&[0xAF, 1]);
            b.extend_from_slice(raw);
            out.push(Tag { kind: TagKind::Audio, ms, body: b.into() });
            n += 1;
            es = &es[len..];
        }
    }
}

/// Annex B's NAL units (start codes of three or four bytes).
fn nals(es: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= es.len() {
        if es[i] == 0 && es[i + 1] == 0 && es[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map_or(es.len(), |n| n - 3);
        // A four-byte start code's leading zero belongs to it, not this unit.
        while end > s && es[end - 1] == 0 {
            end -= 1;
        }
        out.push(&es[s..end]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ts::TsMuxer;
    use crate::ts::tests::sample_tags;

    /// FFmpeg's FLV → our TS → back: the same frames, keyframes and sound, at the same times.
    #[test]
    fn ts_comes_back_as_the_tags_it_was_made_from() {
        let Some(tags) = sample_tags() else { return };
        let mut m = TsMuxer::new();
        let mut ts = Vec::new();
        for (kind, ms, body) in &tags {
            match kind {
                9 => ts.extend(m.video(*ms, body)),
                8 => ts.extend(m.audio(*ms, body)),
                _ => {}
            }
        }
        let mut d = TsDemuxer::new();
        let mut back = Vec::new();
        // Fed in odd pieces, as a network hands it over.
        for chunk in ts.chunks(1000) {
            back.extend(d.push(chunk));
        }
        // The last PES of each finishes when the next starts: one more push of nothing changes
        // nothing, so the count is all but the last of each.
        let frames = |k: u8| tags.iter().filter(|(t, _, b)| *t == k && b.get(1) == Some(&1)).count();
        let vids: Vec<&Tag> = back.iter().filter(|t| t.kind == TagKind::Video && t.body[1] == 1).collect();
        let auds = back.iter().filter(|t| t.kind == TagKind::Audio && t.body[1] == 1).count();
        assert!(vids.len() + 1 >= frames(9) && vids.len() <= frames(9), "{} of {}", vids.len(), frames(9));
        assert!(auds + 2 >= frames(8) && auds <= frames(8), "{auds} of {}", frames(8));
        assert!(back[0].kind == TagKind::Video && back[0].body[..2] == [0x17, 0], "the AVC header first");
        assert_eq!(vids[0].body[0], 0x17, "starts on a keyframe");
        let keys = vids.iter().filter(|t| t.body[0] == 0x17).count();
        let want = tags.iter().filter(|(t, _, b)| *t == 9 && b[0] == 0x17 && b[1] == 1).count();
        assert!(keys + 1 >= want && keys <= want, "{keys} keyframes of {want}");
        // The 10th frame comes back at its time (to the ms), with its picture offset.
        let orig: Vec<&(u8, u32, Vec<u8>)> = tags.iter().filter(|(t, _, b)| *t == 9 && b[1] == 1).collect();
        let first = orig[0].1;
        assert!((i64::from(vids[10].ms) - i64::from(orig[10].1 - first)).abs() <= 1);
        assert_eq!(vids[10].body[2..5], orig[10].2[2..5], "composition offset kept");
        assert!(d.refused().is_none());
        // The headers match what the encoder wrote.
        let asc = tags.iter().find(|(t, _, b)| *t == 8 && b[1] == 0).unwrap();
        let a = back.iter().find(|t| t.kind == TagKind::Audio && t.body[1] == 0).unwrap();
        assert_eq!(a.body[2..4], asc.2[2..4]);
    }

    #[test]
    fn junk_and_cut_packets_are_survived() {
        let mut d = TsDemuxer::new();
        assert!(d.push(&[0x47, 1, 2]).is_empty());
        assert!(d.push(&vec![0xAB; 5000]).is_empty());
        assert!(d.push(&[]).is_empty());
        // A program table shorter than it can be: ignored, never a crash.
        let mut pat = vec![0x47, 0x40, 0x00, 0x10, 0x00, 0x00, 0xB0, 0x01, 0x00];
        pat.resize(PACKET, 0xFF);
        assert!(d.push(&pat).is_empty());
    }
}
