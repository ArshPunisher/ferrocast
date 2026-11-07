//! MPEG-TS from the hub's FLV tags, for the destinations that carry TS (SRT and RIST, as OBS
//! sends them): H.264 or HEVC in Annex B with their parameter sets before each keyframe, AAC as
//! ADTS. One program; PAT and PMT before every keyframe (a receiver joining late starts there),
//! the clock (PCR) on the video.

use crate::flv;

/// A TS packet's size.
pub const PACKET: usize = 188;

const PAT_PID: u16 = 0x0000;
const PMT_PID: u16 = 0x1000;
const VIDEO_PID: u16 = 0x0100;
const AUDIO_PID: u16 = 0x0101;
/// stream_type: H.264, HEVC, AAC in ADTS.
const H264: u8 = 0x1B;
const HEVC: u8 = 0x24;
const ADTS: u8 = 0x0F;
/// Times start here (90 kHz), so a frame shown before it's decoded never goes below zero.
const START: u64 = 90 * 1_000;
/// How far the clock (PCR) runs behind decode times: room for receivers to buffer (300 ms).
const PCR_LEAD: u64 = 90 * 300;

/// MPEG-2's CRC-32 (PAT and PMT).
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for b in data {
        crc ^= u32::from(*b) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04C1_1DB7 } else { crc << 1 };
        }
    }
    crc
}

/// The video's parameter sets, as Annex B, and how long its NAL lengths are.
#[derive(Debug, Clone, Default)]
pub(crate) struct VideoSetup {
    pub(crate) hevc: bool,
    length_size: usize,
    /// SPS and PPS (H.264), or VPS, SPS and PPS (HEVC), each with its start code.
    params: Vec<u8>,
}

/// avcC → SPS and PPS.
fn avcc(rec: &[u8]) -> Option<VideoSetup> {
    let length_size = usize::from(rec.get(4)? & 3) + 1;
    let mut params = Vec::new();
    let mut at = 5;
    let sps = usize::from(rec.get(at)? & 0x1F);
    at += 1;
    for _ in 0..sps {
        let n = usize::from(u16::from_be_bytes([*rec.get(at)?, *rec.get(at + 1)?]));
        params.extend_from_slice(&[0, 0, 0, 1]);
        params.extend_from_slice(rec.get(at + 2..at + 2 + n)?);
        at += 2 + n;
    }
    let pps = usize::from(*rec.get(at)?);
    at += 1;
    for _ in 0..pps {
        let n = usize::from(u16::from_be_bytes([*rec.get(at)?, *rec.get(at + 1)?]));
        params.extend_from_slice(&[0, 0, 0, 1]);
        params.extend_from_slice(rec.get(at + 2..at + 2 + n)?);
        at += 2 + n;
    }
    Some(VideoSetup { hevc: false, length_size, params })
}

/// hvcC → VPS, SPS and PPS (every array it carries).
fn hvcc(rec: &[u8]) -> Option<VideoSetup> {
    let length_size = usize::from(rec.get(21)? & 3) + 1;
    let arrays = usize::from(*rec.get(22)?);
    let mut params = Vec::new();
    let mut at = 23;
    for _ in 0..arrays {
        let count = usize::from(u16::from_be_bytes([*rec.get(at + 1)?, *rec.get(at + 2)?]));
        at += 3;
        for _ in 0..count {
            let n = usize::from(u16::from_be_bytes([*rec.get(at)?, *rec.get(at + 1)?]));
            params.extend_from_slice(&[0, 0, 0, 1]);
            params.extend_from_slice(rec.get(at + 2..at + 2 + n)?);
            at += 2 + n;
        }
    }
    Some(VideoSetup { hevc: true, length_size, params })
}

/// AAC's AudioSpecificConfig → what each ADTS header says: profile, rate index, channels.
fn adts_setup(asc: &[u8]) -> Option<(u8, u8, u8)> {
    let (a, b) = (*asc.first()?, *asc.get(1)?);
    let object = a >> 3;
    let rate = ((a & 7) << 1) | (b >> 7);
    let channels = (b >> 3) & 0x0F;
    (object >= 1 && rate < 13).then_some((object - 1, rate, channels))
}

fn adts_header((profile, rate, channels): (u8, u8, u8), len: usize) -> [u8; 7] {
    let full = len + 7;
    [
        0xFF,
        0xF1,
        (profile << 6) | (rate << 2) | (channels >> 2),
        ((channels & 3) << 6) | ((full >> 11) as u8 & 3),
        (full >> 3) as u8,
        ((full as u8 & 7) << 5) | 0x1F,
        0xFC,
    ]
}

/// A PES timestamp (PTS or DTS) with its 4-bit prefix.
fn pes_time(prefix: u8, t: u64) -> [u8; 5] {
    [
        (prefix << 4) | (((t >> 30) as u8 & 7) << 1) | 1,
        (t >> 22) as u8,
        (((t >> 15) as u8 & 0x7F) << 1) | 1,
        (t >> 7) as u8,
        ((t as u8 & 0x7F) << 1) | 1,
    ]
}

/// The video's setup from its header tag (H.264 or HEVC), or None for anything else.
pub(crate) fn video_setup(header: &[u8]) -> Option<VideoSetup> {
    match flv::video_codec(header) {
        Some(flv::VideoCodec::H264) => header.get(5..).and_then(avcc),
        Some(flv::VideoCodec::Hevc) => header.get(5..).and_then(hvcc),
        _ => None,
    }
}

/// A frame tag as Annex B (with the parameter sets before a keyframe, and an access unit
/// delimiter first when `aud`): whether it's a keyframe, its composition offset (ms), and the
/// bytes. None for anything that isn't a coded frame.
pub(crate) fn annex_b(setup: &VideoSetup, data: &[u8], aud: bool) -> Option<(bool, i32, Vec<u8>)> {
    let key = flv::video_is_keyframe(data);
    let &b = data.first()?;
    // Classic AVC: type, packet type, 3-byte CTS. Enhanced: header, FourCC, then a CTS only for
    // CodedFrames (1), not CodedFramesX (3).
    let (cts, body) = if b & 0x80 != 0 {
        match b & 0x0F {
            1 => (data.get(5..8).map_or(0, cts24), data.get(8..)?),
            3 => (0, data.get(5..)?),
            _ => return None,
        }
    } else {
        if data.get(1) != Some(&1) {
            return None;
        }
        (data.get(2..5).map_or(0, cts24), data.get(5..)?)
    };
    let mut es = Vec::with_capacity(body.len() + setup.params.len() + 16);
    if aud {
        // An access unit delimiter first, as broadcast receivers like.
        if setup.hevc {
            es.extend_from_slice(&[0, 0, 0, 1, 0x46, 0x01, 0x50]);
        } else {
            es.extend_from_slice(&[0, 0, 0, 1, 0x09, 0xF0]);
        }
    }
    if key {
        es.extend_from_slice(&setup.params);
    }
    let mut at = 0;
    while at + setup.length_size <= body.len() {
        let n = body[at..at + setup.length_size].iter().fold(0usize, |n, b| (n << 8) | usize::from(*b));
        at += setup.length_size;
        let Some(nal) = body.get(at..at + n) else { break };
        es.extend_from_slice(&[0, 0, 0, 1]);
        es.extend_from_slice(nal);
        at += n;
    }
    Some((key, cts, es))
}

/// Writes TS as tags come; each call hands back the whole packets it made.
#[derive(Debug, Default)]
pub struct TsMuxer {
    video: Option<VideoSetup>,
    audio: Option<(u8, u8, u8)>,
    /// Continuity counters: PAT, PMT, video, audio.
    cc: [u8; 4],
    /// The first tag's time (ms): times count from it.
    base: Option<u32>,
    /// Program tables sent once at least (and again before each keyframe).
    started: bool,
}

impl TsMuxer {
    pub fn new() -> Self {
        Self::default()
    }

    /// A video tag (FLV body). Headers set the codec up; frames before the first keyframe (or
    /// before the header) are left out.
    pub fn video(&mut self, ts: u32, data: &[u8]) -> Vec<u8> {
        if flv::video_is_header(data) {
            let setup = video_setup(data);
            if setup.is_some() {
                self.video = setup;
                self.started = false;
            }
            return Vec::new();
        }
        let Some(setup) = self.video.clone() else { return Vec::new() };
        let key = flv::video_is_keyframe(data);
        if !self.started && !key {
            return Vec::new();
        }
        let Some((key, cts, es)) = annex_b(&setup, data, true) else { return Vec::new() };
        let dts = self.time(ts);
        let pts = (dts as i64 + i64::from(cts) * 90).max(0) as u64;
        let mut out = Vec::new();
        if key || !self.started {
            out.extend(self.tables());
            self.started = true;
        }
        let mut header = vec![0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0xC0, 10];
        header.extend_from_slice(&pes_time(3, pts));
        header.extend_from_slice(&pes_time(1, dts));
        header.extend_from_slice(&es);
        out.extend(self.packets(VIDEO_PID, 2, &header, Some(dts.saturating_sub(PCR_LEAD)), key));
        out
    }

    /// An audio tag (FLV body): the header sets AAC up; frames go out as ADTS once the video
    /// has started (or at once with no video).
    pub fn audio(&mut self, ts: u32, data: &[u8]) -> Vec<u8> {
        // Only the main track (Twitch's VOD track isn't sent here).
        if flv::audio_extra_track(data).is_some() || data.first().map(|b| b >> 4) != Some(10) {
            return Vec::new();
        }
        match data.get(1) {
            Some(0) => {
                self.audio = data.get(2..).and_then(adts_setup);
                Vec::new()
            }
            Some(1) => {
                let Some(setup) = self.audio else { return Vec::new() };
                if self.video.is_some() && !self.started {
                    return Vec::new();
                }
                let mut out = Vec::new();
                if !self.started {
                    out.extend(self.tables());
                    self.started = true;
                }
                let frame = &data[2..];
                let pts = self.time(ts);
                let len = 8 + frame.len() + 7;
                let mut pes = vec![0x00, 0x00, 0x01, 0xC0, (len >> 8) as u8, len as u8, 0x80, 0x80, 5];
                pes.extend_from_slice(&pes_time(2, pts));
                pes.extend_from_slice(&adts_header(setup, frame.len()));
                pes.extend_from_slice(frame);
                // The clock rides on the audio when there's no video.
                let pcr = self.video.is_none().then_some(pts.saturating_sub(PCR_LEAD));
                out.extend(self.packets(AUDIO_PID, 3, &pes, pcr, false));
                out
            }
            _ => Vec::new(),
        }
    }

    /// Packets were lost on the way here: start again at the next keyframe.
    pub fn resync(&mut self) {
        self.started = false;
    }

    /// A tag's time on the 90 kHz clock.
    fn time(&mut self, ts: u32) -> u64 {
        let base = *self.base.get_or_insert(ts);
        START + u64::from(ts.saturating_sub(base)) * 90
    }

    /// PAT and PMT.
    fn tables(&mut self) -> Vec<u8> {
        let mut pat = vec![0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01];
        pat.extend_from_slice(&[0xE0 | (PMT_PID >> 8) as u8, PMT_PID as u8]);
        let crc = crc32(&pat);
        pat.extend_from_slice(&crc.to_be_bytes());
        let pcr_pid = if self.video.is_some() { VIDEO_PID } else { AUDIO_PID };
        let mut streams = Vec::new();
        if let Some(v) = &self.video {
            streams.extend_from_slice(&[
                if v.hevc { HEVC } else { H264 },
                0xE0 | (VIDEO_PID >> 8) as u8,
                VIDEO_PID as u8,
            ]);
            streams.extend_from_slice(&[0xF0, 0x00]);
        }
        if self.audio.is_some() {
            streams.extend_from_slice(&[ADTS, 0xE0 | (AUDIO_PID >> 8) as u8, AUDIO_PID as u8, 0xF0, 0x00]);
        }
        let len = 9 + streams.len() + 4;
        let mut pmt = vec![0x02, 0xB0 | (len >> 8) as u8, len as u8, 0x00, 0x01, 0xC1, 0x00, 0x00];
        pmt.extend_from_slice(&[0xE0 | (pcr_pid >> 8) as u8, pcr_pid as u8, 0xF0, 0x00]);
        pmt.extend_from_slice(&streams);
        let crc = crc32(&pmt);
        pmt.extend_from_slice(&crc.to_be_bytes());
        let mut out = Vec::with_capacity(2 * PACKET);
        for (pid, cc, section) in [(PAT_PID, 0, pat), (PMT_PID, 1, pmt)] {
            let mut p = vec![0x47, 0x40 | (pid >> 8) as u8, pid as u8, 0x10 | self.next_cc(cc), 0x00];
            p.extend_from_slice(&section);
            p.resize(PACKET, 0xFF);
            out.extend(p);
        }
        out
    }

    fn next_cc(&mut self, i: usize) -> u8 {
        let c = self.cc[i];
        self.cc[i] = (c + 1) & 0x0F;
        c
    }

    /// One PES cut into packets: the first starts the unit (and carries the clock when asked),
    /// the last is filled out with stuffing.
    fn packets(&mut self, pid: u16, cc: usize, pes: &[u8], pcr: Option<u64>, key: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity((pes.len() / 170 + 2) * PACKET);
        let mut at = 0;
        let mut first = true;
        while at < pes.len() {
            let mut adapt = Vec::new();
            if first && (pcr.is_some() || key) {
                let flags = if key { 0x40 } else { 0x00 } | if pcr.is_some() { 0x10 } else { 0x00 };
                adapt.push(flags);
                if let Some(t) = pcr {
                    // PCR base (33 bits), 6 reserved bits, extension 0.
                    adapt.extend_from_slice(&[
                        (t >> 25) as u8,
                        (t >> 17) as u8,
                        (t >> 9) as u8,
                        (t >> 1) as u8,
                        ((t as u8 & 1) << 7) | 0x7E,
                        0x00,
                    ]);
                }
            }
            let head = 4 + if adapt.is_empty() { 0 } else { 1 + adapt.len() };
            let room = PACKET - head;
            let left = pes.len() - at;
            if left < room {
                // Stuffing so the payload ends the packet exactly.
                let pad = room - left;
                if adapt.is_empty() {
                    if pad == 1 {
                        // An adaptation field of length 0 takes exactly one byte.
                        let mut p = vec![0x47, (pid >> 8) as u8 | if first { 0x40 } else { 0 }, pid as u8];
                        p.push(0x30 | self.next_cc(cc));
                        p.push(0);
                        p.extend_from_slice(&pes[at..]);
                        out.extend(p);
                        break;
                    }
                    adapt.push(0x00);
                    adapt.extend(std::iter::repeat_n(0xFF, pad - 2));
                } else {
                    adapt.extend(std::iter::repeat_n(0xFF, pad));
                }
            }
            let take = (PACKET - 4 - if adapt.is_empty() { 0 } else { 1 + adapt.len() }).min(left);
            let mut p = Vec::with_capacity(PACKET);
            p.extend_from_slice(&[0x47, (pid >> 8) as u8 | if first { 0x40 } else { 0 }, pid as u8]);
            let control = if adapt.is_empty() { 0x10 } else { 0x30 };
            p.push(control | self.next_cc(cc));
            if !adapt.is_empty() {
                p.push(adapt.len() as u8);
                p.extend_from_slice(&adapt);
            }
            p.extend_from_slice(&pes[at..at + take]);
            debug_assert_eq!(p.len(), PACKET);
            out.extend(p);
            at += take;
            first = false;
        }
        out
    }
}

/// A signed 24-bit composition time (ms).
fn cts24(b: &[u8]) -> i32 {
    let v = (i32::from(b[0]) << 16) | (i32::from(b[1]) << 8) | i32::from(b[2]);
    if v & 0x80_0000 != 0 { v - 0x100_0000 } else { v }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn crc_is_mpeg2s() {
        // The PAT of a one-program stream, as every muxer writes it.
        let pat = [0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00];
        assert_eq!(crc32(&pat), 0x2AB1_04B2);
    }

    #[test]
    fn adts_says_aac_lc_48k_stereo() {
        let setup = adts_setup(&[0x11, 0x90]).unwrap_or_default();
        assert_eq!(setup, (1, 3, 2));
        let h = adts_header(setup, 100);
        assert_eq!(h[..2], [0xFF, 0xF1]);
        let len = (usize::from(h[3] & 3) << 11) | (usize::from(h[4]) << 3) | usize::from(h[5] >> 5);
        assert_eq!(len, 107);
    }

    #[test]
    fn every_packet_is_whole() {
        let mut m = TsMuxer::new();
        // avcC with one tiny SPS and PPS, 4-byte lengths.
        let rec = [1, 0x64, 0, 0x1F, 0xFF, 0xE1, 0, 2, 0x67, 0x64, 1, 0, 2, 0x68, 0xEE];
        let mut header = vec![0x17, 0, 0, 0, 0];
        header.extend_from_slice(&rec);
        assert!(m.video(0, &header).is_empty());
        for (i, size) in [5usize, 170, 171, 183, 184, 1000].into_iter().enumerate() {
            let mut tag = vec![if i == 0 { 0x17 } else { 0x27 }, 1, 0, 0, 0];
            tag.extend_from_slice(&(size as u32).to_be_bytes());
            tag.extend(std::iter::repeat_n(0x41, size));
            let out = m.video(i as u32 * 33, &tag);
            assert!(!out.is_empty() && out.len().is_multiple_of(PACKET), "{size}: {}", out.len());
            assert!(out.chunks(PACKET).all(|p| p[0] == 0x47));
        }
    }

    /// The tags of an FLV file: (type, ms, body).
    pub(crate) fn flv_tags(file: &[u8]) -> Vec<(u8, u32, Vec<u8>)> {
        let mut out = Vec::new();
        let mut at = 9 + 4;
        while at + 11 <= file.len() {
            let kind = file[at];
            let size = (usize::from(file[at + 1]) << 16) | (usize::from(file[at + 2]) << 8) | usize::from(file[at + 3]);
            let ms = (u32::from(file[at + 7]) << 24)
                | (u32::from(file[at + 4]) << 16)
                | (u32::from(file[at + 5]) << 8)
                | u32::from(file[at + 6]);
            let Some(body) = file.get(at + 11..at + 11 + size) else { break };
            out.push((kind, ms, body.to_vec()));
            at += 11 + size + 4;
        }
        out
    }

    /// Three seconds of FFmpeg's own FLV (H.264 with B-frames, AAC) as tags; None without an
    /// FFmpeg with x264.
    pub(crate) fn sample_tags() -> Option<Vec<(u8, u32, Vec<u8>)>> {
        sample_tags_with(2)
    }

    /// The same with `bframes` B-frames between pictures (0: none, as WebRTC needs).
    pub(crate) fn sample_tags_with(bframes: u32) -> Option<Vec<(u8, u32, Vec<u8>)>> {
        use std::process::Command;
        let path =
            std::env::temp_dir().join(format!("ferrocast-sample-{}-{}.flv", std::process::id(), fastrand::u32(..)));
        let made = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=320x180:rate=30", "-f", "lavfi"])
            .args(["-i", "sine=frequency=440", "-t", "3", "-c:v", "libx264", "-g", "30", "-c:a", "aac"])
            .args(["-bf", &bframes.to_string()])
            .arg(&path)
            .status();
        if !made.is_ok_and(|s| s.success()) {
            eprintln!("skipped: no ffmpeg with x264 here");
            return None;
        }
        let file = std::fs::read(&path).ok();
        let _ = std::fs::remove_file(&path);
        file.map(|f| flv_tags(&f))
    }

    /// What ffprobe reads in TS bytes: "codec,frames codec,frames", and its complaints.
    pub(crate) fn probe(ts: &[u8]) -> Option<String> {
        use std::process::Command;
        let path =
            std::env::temp_dir().join(format!("ferrocast-probe-{}-{}.ts", std::process::id(), fastrand::u32(..)));
        std::fs::write(&path, ts).ok()?;
        let out = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-count_frames",
                "-show_entries",
                "stream=codec_name,nb_read_frames",
                "-of",
                "csv=p=0",
            ])
            .arg(&path)
            .output();
        let _ = std::fs::remove_file(&path);
        let out = out.ok()?;
        let got = String::from_utf8_lossy(&out.stdout).trim().replace('\n', " ");
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Some(if err.is_empty() { got } else { format!("{got} / {err}") })
    }

    /// FFmpeg's own FLV through the muxer, read back by ffprobe: every frame, no complaints.
    #[test]
    fn ffmpeg_reads_what_it_wrote_back() {
        let Some(tags) = sample_tags() else { return };
        let mut m = TsMuxer::new();
        let mut ts = Vec::new();
        for (kind, ms, body) in tags {
            match kind {
                9 => ts.extend(m.video(ms, &body)),
                8 => ts.extend(m.audio(ms, &body)),
                _ => {}
            }
        }
        assert_eq!(ts.len() % PACKET, 0);
        let Some(got) = probe(&ts) else { return };
        assert!(got.starts_with("h264,90 aac,") && !got.contains(" / "), "{got}");
    }
}
