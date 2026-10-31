//! Crash-safe recording: fragmented MP4. The header (`moov`) goes first with no samples in it,
//! then a fragment (`moof` + `mdat`) at every video keyframe. Each fragment is whole on disk once
//! written, so a crash or a pulled plug loses at most the fragment being built (about the
//! keyframe interval), and the file plays in every player as it is: no repair, no remux.

use std::io::{self, Write};

use crate::audio::{AAC_FRAME, AudioPacket};
use crate::video::{VideoCodec, VideoPacket};

/// Video's clock: 90 kHz, as broadcast uses.
const VIDEO_SCALE: u32 = 90_000;
const VIDEO: u32 = 1;
const AUDIO: u32 = 2;
/// Sample flags: a keyframe (depends on nothing); any other frame (depends on others, not sync).
const KEY_FLAGS: u32 = 0x0200_0000;
const DELTA_FLAGS: u32 = 0x0101_0000;
/// Audio-only recordings cut a fragment this often (in audio frames, ~1 s at 48 kHz).
const AUDIO_ONLY_FRAGMENT: usize = 47;

/// The video track's shape.
#[derive(Debug, Clone)]
pub struct VideoTrack {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    /// avcC or hvcC (the encoder's header).
    pub config: Vec<u8>,
}

/// The audio track's shape (AAC).
#[derive(Debug, Clone)]
pub struct AudioTrack {
    pub sample_rate: u32,
    pub channels: u32,
    /// The AudioSpecificConfig.
    pub config: Vec<u8>,
    pub bitrate_kbps: u32,
}

/// A sample's line in a fragment: duration, size, flags, composition offset.
type Row = (u32, u32, u32, i32);

#[derive(Debug, Clone)]
struct Sample {
    dts: i64,
    cts: i64,
    key: bool,
    data: Vec<u8>,
}

/// Writes a fragmented MP4 to `out` as packets come.
#[derive(Debug)]
pub struct Mp4Writer<W: Write> {
    out: W,
    video: Option<VideoTrack>,
    audio: Option<AudioTrack>,
    vq: Vec<Sample>,
    aq: Vec<Sample>,
    /// Each track's next decode time (its own clock): where a fragment starts.
    v_next: Option<i64>,
    a_next: Option<i64>,
    /// The first video decode time, so the file starts at 0.
    v_start: Option<i64>,
    sequence: u32,
    /// Video's last sample duration, for the fragment's final frame.
    v_last_dur: u32,
}

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(body.len() + 8);
    v.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
    v.extend_from_slice(kind);
    v.extend_from_slice(body);
    v
}

fn full(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(body.len() + 4);
    b.push(version);
    b.extend_from_slice(&flags.to_be_bytes()[1..]);
    b.extend_from_slice(body);
    boxed(kind, &b)
}

fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.concat()
}

const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn matrix() -> Vec<u8> {
    MATRIX.iter().flat_map(|v| v.to_be_bytes()).collect()
}

impl<W: Write> Mp4Writer<W> {
    /// Writes the header at once: the file is valid (empty) from here.
    pub fn new(mut out: W, video: Option<VideoTrack>, audio: Option<AudioTrack>) -> io::Result<Self> {
        if video.is_none() && audio.is_none() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "a recording needs video or sound"));
        }
        let mut ftyp = b"iso6".to_vec();
        ftyp.extend_from_slice(&0u32.to_be_bytes());
        for b in [b"iso6", b"isom", b"iso2", b"mp41"] {
            ftyp.extend_from_slice(b);
        }
        if video.as_ref().is_some_and(|v| v.codec == VideoCodec::Hevc) {
            ftyp.extend_from_slice(b"hvc1");
        } else {
            ftyp.extend_from_slice(b"avc1");
        }
        out.write_all(&boxed(b"ftyp", &ftyp))?;
        out.write_all(&moov(video.as_ref(), audio.as_ref()))?;
        out.flush()?;
        Ok(Self {
            out,
            video,
            audio,
            vq: Vec::new(),
            aq: Vec::new(),
            v_next: None,
            a_next: None,
            v_start: None,
            sequence: 0,
            v_last_dur: 0,
        })
    }

    /// A video frame. A keyframe first writes out what came before it.
    pub fn video(&mut self, p: &VideoPacket) -> io::Result<()> {
        if self.video.is_none() {
            return Ok(());
        }
        let start = *self.v_start.get_or_insert(p.dts_us);
        if p.key && !self.vq.is_empty() {
            self.fragment()?;
        }
        if self.vq.is_empty() && !p.key && self.sequence == 0 {
            // Nothing decodes before the first keyframe.
            return Ok(());
        }
        let ticks = |us: i64| (us - start) * i64::from(VIDEO_SCALE) / 1_000_000;
        self.vq.push(Sample {
            dts: ticks(p.dts_us),
            cts: ticks(p.pts_us) - ticks(p.dts_us),
            key: p.key,
            data: p.data.clone(),
        });
        Ok(())
    }

    /// An AAC frame.
    pub fn audio(&mut self, p: &AudioPacket) -> io::Result<()> {
        let Some(a) = &self.audio else { return Ok(()) };
        let start = self.v_start.unwrap_or(0);
        if self.video.is_some() && self.v_start.is_none() {
            // Sound before the first picture waits out (it'd start the file early).
            return Ok(());
        }
        if p.pts_us < start {
            return Ok(());
        }
        let ticks = (p.pts_us - start) * i64::from(a.sample_rate) / 1_000_000;
        self.aq.push(Sample { dts: ticks, cts: 0, key: true, data: p.data.clone() });
        if self.video.is_none() && self.aq.len() >= AUDIO_ONLY_FRAGMENT {
            self.fragment()?;
        }
        Ok(())
    }

    /// Writes out what's waiting and hands the output back.
    pub fn finish(mut self) -> io::Result<W> {
        if !self.vq.is_empty() || !self.aq.is_empty() {
            self.fragment()?;
        }
        self.out.flush()?;
        Ok(self.out)
    }

    fn fragment(&mut self) -> io::Result<()> {
        self.sequence += 1;
        let vq = std::mem::take(&mut self.vq);
        let aq = std::mem::take(&mut self.aq);
        let mut trafs: Vec<(u32, i64, Vec<Row>)> = Vec::new();
        let mut payload = Vec::new();
        if !vq.is_empty() {
            let mut rows = Vec::new();
            for (i, s) in vq.iter().enumerate() {
                let dur = match vq.get(i + 1) {
                    Some(n) => (n.dts - s.dts).max(1) as u32,
                    None if self.v_last_dur > 0 => self.v_last_dur,
                    None => VIDEO_SCALE / 30,
                };
                self.v_last_dur = dur;
                rows.push((dur, s.data.len() as u32, if s.key { KEY_FLAGS } else { DELTA_FLAGS }, s.cts as i32));
                payload.extend_from_slice(&s.data);
            }
            let base = self.v_next.unwrap_or(vq[0].dts).max(vq[0].dts);
            trafs.push((VIDEO, base, rows));
            self.v_next = Some(vq.last().map_or(base, |s| s.dts) + i64::from(self.v_last_dur));
        }
        if !aq.is_empty() {
            let mut rows = Vec::new();
            for s in &aq {
                rows.push((AAC_FRAME as u32, s.data.len() as u32, KEY_FLAGS, 0));
                payload.extend_from_slice(&s.data);
            }
            let base = self.a_next.unwrap_or(aq[0].dts).max(aq[0].dts);
            trafs.push((AUDIO, base, rows));
            self.a_next = Some(base + aq.len() as i64 * AAC_FRAME as i64);
        }
        // The moof's size is needed for each trun's data offset: build it twice.
        let build = |offsets: &[u32]| -> Vec<u8> {
            let mut parts = vec![full(b"mfhd", 0, 0, &self.sequence.to_be_bytes())];
            for (i, (track, base, rows)) in trafs.iter().enumerate() {
                let tfhd = full(b"tfhd", 0, 0x02_0000, &track.to_be_bytes());
                let tfdt = full(b"tfdt", 1, 0, &(*base as u64).to_be_bytes());
                let mut t = Vec::with_capacity(8 + rows.len() * 16);
                t.extend_from_slice(&(rows.len() as u32).to_be_bytes());
                t.extend_from_slice(&offsets.get(i).copied().unwrap_or(0).to_be_bytes());
                for (dur, size, flags, cts) in rows {
                    t.extend_from_slice(&dur.to_be_bytes());
                    t.extend_from_slice(&size.to_be_bytes());
                    t.extend_from_slice(&flags.to_be_bytes());
                    t.extend_from_slice(&cts.to_be_bytes());
                }
                // Data offset, and per sample: duration, size, flags, composition offset.
                let trun = full(b"trun", 1, 0x01 | 0x100 | 0x200 | 0x400 | 0x800, &t);
                parts.push(boxed(b"traf", &cat(&[tfhd, tfdt, trun])));
            }
            boxed(b"moof", &cat(&parts))
        };
        let size = build(&[0, 0]).len() as u32;
        let mut offsets = Vec::new();
        let mut at = size + 8;
        for (_, _, rows) in &trafs {
            offsets.push(at);
            at += rows.iter().map(|r| r.1).sum::<u32>();
        }
        let moof = build(&offsets);
        self.out.write_all(&moof)?;
        self.out.write_all(&(payload.len() as u32 + 8).to_be_bytes())?;
        self.out.write_all(b"mdat")?;
        self.out.write_all(&payload)?;
        self.out.flush()
    }
}

fn moov(video: Option<&VideoTrack>, audio: Option<&AudioTrack>) -> Vec<u8> {
    let mut mvhd = vec![0u8; 8];
    mvhd.extend_from_slice(&1000u32.to_be_bytes());
    mvhd.extend_from_slice(&0u32.to_be_bytes());
    mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    mvhd.extend_from_slice(&0x0100u16.to_be_bytes());
    mvhd.extend_from_slice(&[0; 10]);
    mvhd.extend_from_slice(&matrix());
    mvhd.extend_from_slice(&[0; 24]);
    mvhd.extend_from_slice(&3u32.to_be_bytes());
    let mut parts = vec![full(b"mvhd", 0, 0, &mvhd)];
    let mut trex = Vec::new();
    if let Some(v) = video {
        parts.push(video_trak(v));
        trex.push(trex_box(VIDEO));
    }
    if let Some(a) = audio {
        parts.push(audio_trak(a));
        trex.push(trex_box(AUDIO));
    }
    parts.push(boxed(b"mvex", &cat(&trex)));
    boxed(b"moov", &cat(&parts))
}

fn trex_box(track: u32) -> Vec<u8> {
    let mut b = track.to_be_bytes().to_vec();
    b.extend_from_slice(&1u32.to_be_bytes());
    b.extend_from_slice(&[0; 12]);
    full(b"trex", 0, 0, &b)
}

fn tkhd(track: u32, audio: bool, w: u32, h: u32) -> Vec<u8> {
    let mut b = vec![0u8; 8];
    b.extend_from_slice(&track.to_be_bytes());
    b.extend_from_slice(&[0; 4]);
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&[0; 8]);
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&(if audio { 0x0100u16 } else { 0 }).to_be_bytes());
    b.extend_from_slice(&[0; 2]);
    b.extend_from_slice(&matrix());
    b.extend_from_slice(&(w << 16).to_be_bytes());
    b.extend_from_slice(&(h << 16).to_be_bytes());
    // Enabled, in the movie, in the preview.
    full(b"tkhd", 0, 3, &b)
}

fn mdhd(scale: u32) -> Vec<u8> {
    let mut b = vec![0u8; 8];
    b.extend_from_slice(&scale.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    // Language "und".
    b.extend_from_slice(&0x55C4u16.to_be_bytes());
    b.extend_from_slice(&[0; 2]);
    full(b"mdhd", 0, 0, &b)
}

fn hdlr(kind: &[u8; 4], name: &str) -> Vec<u8> {
    let mut b = vec![0u8; 4];
    b.extend_from_slice(kind);
    b.extend_from_slice(&[0; 12]);
    b.extend_from_slice(name.as_bytes());
    b.push(0);
    full(b"hdlr", 0, 0, &b)
}

fn dinf() -> Vec<u8> {
    let url = full(b"url ", 0, 1, &[]);
    let mut dref = 1u32.to_be_bytes().to_vec();
    dref.extend_from_slice(&url);
    boxed(b"dinf", &full(b"dref", 0, 0, &dref))
}

/// An stbl with the sample entry and the empty tables fragments need.
fn stbl(entry: Vec<u8>) -> Vec<u8> {
    let mut stsd = 1u32.to_be_bytes().to_vec();
    stsd.extend_from_slice(&entry);
    let empty = 0u32.to_be_bytes();
    cat(&[
        full(b"stsd", 0, 0, &stsd),
        full(b"stts", 0, 0, &empty),
        full(b"stsc", 0, 0, &empty),
        full(b"stsz", 0, 0, &[0; 8]),
        full(b"stco", 0, 0, &empty),
    ])
}

fn video_trak(v: &VideoTrack) -> Vec<u8> {
    let mut e = vec![0u8; 6];
    e.extend_from_slice(&1u16.to_be_bytes());
    e.extend_from_slice(&[0; 16]);
    e.extend_from_slice(&(v.width as u16).to_be_bytes());
    e.extend_from_slice(&(v.height as u16).to_be_bytes());
    e.extend_from_slice(&0x0048_0000u32.to_be_bytes());
    e.extend_from_slice(&0x0048_0000u32.to_be_bytes());
    e.extend_from_slice(&[0; 4]);
    e.extend_from_slice(&1u16.to_be_bytes());
    e.extend_from_slice(&[0; 32]);
    e.extend_from_slice(&0x0018u16.to_be_bytes());
    e.extend_from_slice(&0xFFFFu16.to_be_bytes());
    let (entry, config) = match v.codec {
        VideoCodec::H264 => (b"avc1", b"avcC"),
        VideoCodec::Hevc => (b"hvc1", b"hvcC"),
    };
    e.extend_from_slice(&boxed(config, &v.config));
    // Rec. 709, limited range: nclx colour.
    let mut colr = b"nclx".to_vec();
    colr.extend_from_slice(&[0, 1, 0, 1, 0, 1, 0]);
    e.extend_from_slice(&boxed(b"colr", &colr));
    let minf = boxed(b"minf", &cat(&[full(b"vmhd", 0, 1, &[0; 8]), dinf(), boxed(b"stbl", &stbl(boxed(entry, &e)))]));
    let mdia = boxed(b"mdia", &cat(&[mdhd(VIDEO_SCALE), hdlr(b"vide", "Ferrocast video"), minf]));
    boxed(b"trak", &cat(&[tkhd(VIDEO, false, v.width, v.height), mdia]))
}

/// An MPEG-4 descriptor: tag, size (one byte while small), body.
fn descriptor(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![tag];
    let n = body.len() as u32;
    v.extend_from_slice(&[
        0x80 | ((n >> 21) & 0x7F) as u8,
        0x80 | ((n >> 14) & 0x7F) as u8,
        0x80 | ((n >> 7) & 0x7F) as u8,
        (n & 0x7F) as u8,
    ]);
    v.extend_from_slice(body);
    v
}

fn audio_trak(a: &AudioTrack) -> Vec<u8> {
    let mut e = vec![0u8; 6];
    e.extend_from_slice(&1u16.to_be_bytes());
    e.extend_from_slice(&[0; 8]);
    e.extend_from_slice(&(a.channels as u16).to_be_bytes());
    e.extend_from_slice(&16u16.to_be_bytes());
    e.extend_from_slice(&[0; 4]);
    e.extend_from_slice(&((a.sample_rate.min(65535)) << 16).to_be_bytes());
    // ES descriptor: ES id 2, then the decoder config (AAC 0x40, audio stream 0x15) and the
    // AudioSpecificConfig, then the SL config.
    let bits = a.bitrate_kbps * 1000;
    let mut dc = vec![0x40, 0x15, 0, 0, 0];
    dc.extend_from_slice(&bits.to_be_bytes());
    dc.extend_from_slice(&bits.to_be_bytes());
    dc.extend_from_slice(&descriptor(0x05, &a.config));
    let mut es = vec![0, AUDIO as u8, 0];
    es.extend_from_slice(&descriptor(0x04, &dc));
    es.extend_from_slice(&descriptor(0x06, &[0x02]));
    e.extend_from_slice(&full(b"esds", 0, 0, &descriptor(0x03, &es)));
    let minf = boxed(b"minf", &cat(&[full(b"smhd", 0, 0, &[0; 4]), dinf(), boxed(b"stbl", &stbl(boxed(b"mp4a", &e)))]));
    let mdia = boxed(b"mdia", &cat(&[mdhd(a.sample_rate), hdlr(b"soun", "Ferrocast sound"), minf]));
    boxed(b"trak", &cat(&[tkhd(AUDIO, true, 0, 0), mdia]))
}
