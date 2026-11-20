//! Crash-safe recording: fragmented MP4. The header (`moov`) goes first with no samples in it,
//! then a fragment (`moof` + `mdat`) at every video keyframe. Each fragment is whole on disk once
//! written, so a crash or a pulled plug loses at most the fragment being built (about the
//! keyframe interval), and the file plays in every player as it is: no repair, no remux.
//!
//! A clean stop (`finalize`) makes it a regular MP4, as OBS's hybrid MP4 does: the sample tables
//! kept in memory while recording are written as a full `moov` at the end of the file (real
//! durations, `stts`/`stss`/`stsz`/`stsc`/`co64`), the fragments' `moof` boxes and the first
//! header become `free` boxes in place (four bytes each), and the samples stay where they are,
//! so there is no copy and it takes milliseconds. Premiere, Final Cut, DaVinci Resolve and
//! QuickTime then open a normal MP4 that seeks fast. A `.mov` gets QuickTime's own brand and
//! handler strings. Until then, and after a crash, it is the fragmented file above.

use std::io::{self, Seek, SeekFrom, Write};

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
/// Room kept after the header (a `free` box) for the chapter marks, written there at the end
/// (about 300 marks with short names).
pub(crate) const ROOM: usize = 16 * 1024;
/// The most chapter marks a file takes (MP4's list counts them in a byte).
pub const MAX_CHAPTERS: usize = 255;

/// A chapter mark's name: "Chapter n" when it has none, at most `max` bytes (whole letters).
pub(crate) fn chapter_name(name: &str, n: usize, max: usize) -> String {
    let name = name.trim();
    let mut s = if name.is_empty() { format!("Chapter {n}") } else { name.to_string() };
    while s.len() > max {
        s.pop();
    }
    s
}

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
    /// The track's name in the file ("Mic", "Game"); empty: "Ferrocast sound".
    pub name: String,
}

/// A sample's line in a fragment: duration, size, flags, composition offset.
type Row = (u32, u32, u32, i32);

/// MP4, or QuickTime's MOV (its brand and handler strings are written when the file is finished).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Flavor {
    #[default]
    Mp4,
    Mov,
}

/// What a finished file needs of one track, kept from each fragment as it is written: the
/// sample tables of a regular MP4.
#[derive(Debug, Default, Clone)]
struct Table {
    /// Each sample's duration.
    durs: Vec<u32>,
    sizes: Vec<u32>,
    /// Composition offsets (video only).
    cts: Vec<i32>,
    /// Keyframes, as 1-based sample numbers (video only).
    keys: Vec<u32>,
    /// A chunk per fragment: where its first sample is in the file, and how many samples.
    chunks: Vec<(u64, u32)>,
    /// Where the track's first fragment starts on its clock (a sound that came after the
    /// picture): an empty edit makes it start there.
    first: Option<i64>,
    /// Where the samples so far end on its clock.
    next: i64,
}

impl Table {
    /// One fragment's samples for this track, `at` bytes into the file, starting at `base`.
    fn add(&mut self, at: u64, base: i64, rows: &[Row], video: bool) {
        if self.first.is_none() {
            self.first = Some(base);
            self.next = base;
        }
        // Each fragment starts where its header says (`base`), so the file's times are the
        // fragmented file's: a gap before it is held by the last sample, an overlap (the last
        // frames of the one before reach past it) is taken off the end of what came before.
        if base > self.next {
            if let Some(last) = self.durs.last_mut() {
                *last = last.saturating_add(u32::try_from(base - self.next).unwrap_or(u32::MAX));
            }
        } else if base < self.next {
            let mut over = self.next - base;
            for d in self.durs.iter_mut().rev() {
                let take = i64::from(*d - 1).min(over);
                *d -= take as u32;
                over -= take;
                if over == 0 {
                    break;
                }
            }
        }
        self.next = base;
        for r in rows {
            self.durs.push(r.0);
            self.sizes.push(r.1);
            if video {
                self.cts.push(r.3);
                if r.2 == KEY_FLAGS {
                    self.keys.push(self.sizes.len() as u32);
                }
            }
            self.next += i64::from(r.0);
        }
        self.chunks.push((at, rows.len() as u32));
    }

    /// The media's length in the track's own clock.
    fn total(&self) -> u64 {
        self.durs.iter().map(|d| u64::from(*d)).sum()
    }

    fn samples(&self) -> usize {
        self.sizes.len()
    }

    /// Runs of (samples, duration), as `stts` has them.
    fn runs(&self) -> Vec<(u32, u32)> {
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for d in &self.durs {
            match runs.last_mut() {
                Some(last) if last.1 == *d => last.0 += 1,
                _ => runs.push((1, *d)),
            }
        }
        runs
    }
}

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
    /// Sound tracks, first to last (track ids 2, 3…; the first plays by default).
    audio: Vec<AudioTrack>,
    vq: Vec<Sample>,
    aq: Vec<Vec<Sample>>,
    /// Each track's next decode time (its own clock): where a fragment starts.
    v_next: Option<i64>,
    a_next: Vec<Option<i64>>,
    /// The first video decode time, so the file starts at 0.
    v_start: Option<i64>,
    sequence: u32,
    /// Video's last sample duration, for the fragment's final frame.
    v_last_dur: u32,
    /// Bytes written so far.
    written: u64,
    /// The header as written (after `ftyp`), to write again with the chapter marks.
    moov: Vec<u8>,
    ftyp_len: u64,
    /// The `ftyp` as written (a MOV replaces it when it is finished, and puts it back if that fails).
    ftyp: Vec<u8>,
    /// Chapter marks: where (µs from the file's start) and their names.
    chapters: Vec<(i64, String)>,
    /// The latest time written (µs from the file's start): where a mark goes.
    last_us: i64,
    flavor: Flavor,
    /// The sample tables a finished file needs: the picture's, then each sound track's.
    tables: Vec<Table>,
    /// Where each fragment's `moof` starts, so a finished file can turn them into `free` boxes.
    moofs: Vec<u64>,
}

/// A finish that failed: the error, the output as it is, and where the last whole fragment
/// ends (the caller cuts the file back there, and what is left plays as a fragmented MP4).
#[derive(Debug)]
pub struct FinishFailed<W> {
    pub error: io::Error,
    pub out: W,
    pub whole: u64,
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
    pub fn new(out: W, video: Option<VideoTrack>, audio: Option<AudioTrack>) -> io::Result<Self> {
        Self::with_tracks(out, video, audio.into_iter().collect())
    }

    /// With several sound tracks (OBS records up to six): `audio_on` fills each.
    pub fn with_tracks(out: W, video: Option<VideoTrack>, audio: Vec<AudioTrack>) -> io::Result<Self> {
        Self::with_flavor(out, video, audio, Flavor::Mp4)
    }

    /// As `with_tracks`, for MP4 or MOV (a MOV is made QuickTime's own when it is finished).
    pub fn with_flavor(
        mut out: W,
        video: Option<VideoTrack>,
        audio: Vec<AudioTrack>,
        flavor: Flavor,
    ) -> io::Result<Self> {
        if video.is_none() && audio.is_empty() {
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
        let (ftyp, moov) =
            (boxed(b"ftyp", &ftyp), moov(video.as_ref(), &audio, &Shape { tables: None, mov: false, chapters: &[] }));
        let tracks = audio.len();
        out.write_all(&ftyp)?;
        out.write_all(&moov)?;
        let mut free = boxed(b"free", &[]);
        free[..4].copy_from_slice(&(ROOM as u32).to_be_bytes());
        free.resize(ROOM, 0);
        out.write_all(&free)?;
        out.flush()?;
        let written = (ftyp.len() + moov.len() + ROOM) as u64;
        let ftyp_len = ftyp.len() as u64;
        let ftyp_bytes = ftyp.clone();
        Ok(Self {
            out,
            video,
            audio,
            vq: Vec::new(),
            aq: vec![Vec::new(); tracks],
            v_next: None,
            a_next: vec![None; tracks],
            v_start: None,
            sequence: 0,
            v_last_dur: 0,
            written,
            moov,
            ftyp_len,
            ftyp: ftyp_bytes,
            chapters: Vec::new(),
            last_us: 0,
            flavor,
            tables: vec![Table::default(); 1 + tracks],
            moofs: Vec::new(),
        })
    }

    /// A chapter mark here (OBS's "Add Chapter Marker"), named, or "Chapter n" when not.
    /// False when the file has `MAX_CHAPTERS` already.
    pub fn chapter(&mut self, name: &str) -> bool {
        if self.chapters.len() >= MAX_CHAPTERS {
            return false;
        }
        let name = chapter_name(name, self.chapters.len() + 1, 255);
        self.chapters.push((self.last_us, name));
        true
    }

    /// A video frame. A keyframe first writes out what came before it.
    pub fn video(&mut self, p: &VideoPacket) -> io::Result<()> {
        if self.video.is_none() {
            return Ok(());
        }
        let start = *self.v_start.get_or_insert(p.dts_us);
        let ticks = |us: i64| (us - start) * i64::from(VIDEO_SCALE) / 1_000_000;
        if p.key && !self.vq.is_empty() {
            self.fragment()?;
        }
        if self.vq.is_empty() && !p.key && self.sequence == 0 {
            // Nothing decodes before the first keyframe.
            return Ok(());
        }
        self.last_us = self.last_us.max(p.pts_us - start);
        self.vq.push(Sample {
            dts: ticks(p.dts_us),
            cts: ticks(p.pts_us) - ticks(p.dts_us),
            key: p.key,
            data: p.data.clone(),
        });
        Ok(())
    }

    /// An AAC frame for the first sound track.
    pub fn audio(&mut self, p: &AudioPacket) -> io::Result<()> {
        self.audio_on(0, p)
    }

    /// An AAC frame for sound track `track` (0 is the first).
    pub fn audio_on(&mut self, track: usize, p: &AudioPacket) -> io::Result<()> {
        let Some(a) = self.audio.get(track) else { return Ok(()) };
        let start = self.v_start.unwrap_or(0);
        if self.video.is_some() && self.v_start.is_none() {
            // Sound before the first picture waits out (it'd start the file early).
            return Ok(());
        }
        if p.pts_us < start {
            return Ok(());
        }
        self.last_us = self.last_us.max(p.pts_us - start);
        let ticks = (p.pts_us - start) * i64::from(a.sample_rate) / 1_000_000;
        self.aq[track].push(Sample { dts: ticks, cts: 0, key: true, data: p.data.clone() });
        if self.video.is_none() && self.aq[0].len() >= AUDIO_ONLY_FRAGMENT {
            self.fragment()?;
        }
        Ok(())
    }

    /// Bytes written to the output so far (what's still waiting for its fragment isn't).
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Gives the output up without writing what waits (a disk that can't take it): the output,
    /// and where the last whole fragment ends. Bytes of a fragment that was cut short lie past
    /// that point, so the caller cuts the file back there and what is left plays.
    pub fn abandon(self) -> (W, u64) {
        (self.out, self.written)
    }

    /// Writes out what's waiting and hands the output back (chapter marks need
    /// `finish_with_chapters`).
    pub fn finish(mut self) -> io::Result<W> {
        if !self.vq.is_empty() || self.aq.iter().any(|q| !q.is_empty()) {
            self.fragment()?;
        }
        self.out.flush()?;
        Ok(self.out)
    }

    fn fragment(&mut self) -> io::Result<()> {
        self.sequence += 1;
        let vq = std::mem::take(&mut self.vq);
        let aqs: Vec<Vec<Sample>> = self.aq.iter_mut().map(std::mem::take).collect();
        let mut trafs: Vec<(u32, i64, Vec<Row>)> = Vec::new();
        // For the finished file's tables: each track's table, where its samples start in the
        // payload, where it starts on its clock, and its rows.
        let mut adds: Vec<(usize, usize, i64, Vec<Row>)> = Vec::new();
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
            let sum_dur: i64 = rows.iter().map(|r| i64::from(r.0)).sum();
            adds.push((0, 0, base, rows.clone()));
            trafs.push((VIDEO, base, rows));
            self.v_next = Some(base + sum_dur);
        }
        for (i, aq) in aqs.iter().enumerate().filter(|(_, q)| !q.is_empty()) {
            let mut rows = Vec::new();
            let starts = payload.len();
            for s in aq {
                rows.push((AAC_FRAME as u32, s.data.len() as u32, KEY_FLAGS, 0));
                payload.extend_from_slice(&s.data);
            }
            let base = self.a_next[i].unwrap_or(aq[0].dts).max(aq[0].dts);
            adds.push((1 + i, starts, base, rows.clone()));
            trafs.push((AUDIO + i as u32, base, rows));
            self.a_next[i] = Some(base + aq.len() as i64 * AAC_FRAME as i64);
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
        let size = build(&vec![0; trafs.len()]).len() as u32;
        let mut offsets = Vec::new();
        let mut at = size + 8;
        for (_, _, rows) in &trafs {
            offsets.push(at);
            at += rows.iter().map(|r| r.1).sum::<u32>();
        }
        let moof = build(&offsets);
        let at = self.written;
        self.out.write_all(&moof)?;
        self.out.write_all(&(payload.len() as u32 + 8).to_be_bytes())?;
        self.out.write_all(b"mdat")?;
        self.out.write_all(&payload)?;
        self.out.flush()?;
        // Counted once it is all out: `written` is where the last whole fragment ends, which is
        // where a file that ran out of room is cut back to (`abandon`), and what the finished
        // file's tables point at.
        let data_at = at + moof.len() as u64 + 8;
        for (table, starts, base, rows) in adds {
            self.tables[table].add(data_at + starts as u64, base, &rows, table == 0);
        }
        self.moofs.push(at);
        self.written += (moof.len() + payload.len() + 8) as u64;
        Ok(())
    }
}

impl<W: Write + Seek> Mp4Writer<W> {
    /// As `finalize`, with the error flattened (the writer is dropped: nothing is cut back).
    pub fn finish_with_chapters(self) -> io::Result<W> {
        self.finalize().map_err(|f| f.error)
    }

    /// Writes out what's waiting and makes the file a regular MP4 (or MOV): the sample tables as
    /// a full `moov` at the end, real durations, the chapter marks in it, and the fragments'
    /// `moof` boxes and the first header turned into `free` boxes in place. Nothing is copied.
    /// If any of it fails the file is as it was, a fragmented MP4 that plays, and the failure
    /// hands back the output and where its last whole fragment ends, to cut back to.
    pub fn finalize(mut self) -> Result<W, FinishFailed<W>> {
        macro_rules! fail {
            ($self:ident, $e:expr) => {{
                let whole = $self.written;
                return Err(FinishFailed { error: $e, out: $self.out, whole });
            }};
        }
        if (!self.vq.is_empty() || self.aq.iter().any(|q| !q.is_empty()))
            && let Err(e) = self.fragment()
        {
            fail!(self, e);
        }
        if let Err(e) = self.out.flush() {
            fail!(self, e);
        }
        if self.tables.iter().all(|t| t.samples() == 0) {
            // Nothing was recorded: there is nothing to make regular.
            return Ok(self.out);
        }
        let mov = self.flavor == Flavor::Mov;
        let moov = moov(
            self.video.as_ref(),
            &self.audio,
            &Shape { tables: Some(&self.tables), mov, chapters: &self.chapters },
        );
        // 1. The new header at the end. Failing here leaves the fragmented file with a piece of
        //    header past its last fragment, which the caller cuts off.
        let end = self.written;
        let wrote =
            self.out.seek(SeekFrom::Start(end)).and_then(|_| self.out.write_all(&moov)).and_then(|()| self.out.flush());
        if let Err(e) = wrote {
            fail!(self, e);
        }
        // 2. The fragments' headers become `free` boxes (their data stays, the new header's
        //    tables point at it); undone if one can't be written.
        let hide = |out: &mut W, at: u64, kind: &[u8; 4]| -> io::Result<()> {
            out.seek(SeekFrom::Start(at + 4))?;
            out.write_all(kind)
        };
        for (n, at) in self.moofs.iter().enumerate() {
            if let Err(e) = hide(&mut self.out, *at, b"free") {
                for undo in &self.moofs[..n] {
                    let _ = hide(&mut self.out, *undo, b"moof");
                }
                let _ = self.out.flush();
                fail!(self, e);
            }
        }
        // 3. The first header (and the room after it) becomes one `free` box, and a MOV gets
        //    QuickTime's brand in the room `ftyp` had: this is the step that makes the new header the one.
        let gone = (self.moov.len() + ROOM) as u32;
        let flip = (|| -> io::Result<()> {
            self.out.seek(SeekFrom::Start(self.ftyp_len))?;
            self.out.write_all(&gone.to_be_bytes())?;
            self.out.write_all(b"free")?;
            if mov {
                self.out.seek(SeekFrom::Start(0))?;
                self.out.write_all(&qt_ftyp(self.ftyp_len as usize))?;
            }
            self.out.seek(SeekFrom::End(0))?;
            self.out.flush()
        })();
        if let Err(e) = flip {
            // Everything put back: the fragments, the first header, the first `ftyp`.
            for undo in &self.moofs {
                let _ = hide(&mut self.out, *undo, b"moof");
            }
            let old = self.moov.len() as u32;
            let _ = (|| -> io::Result<()> {
                self.out.seek(SeekFrom::Start(self.ftyp_len))?;
                self.out.write_all(&old.to_be_bytes())?;
                self.out.write_all(b"moov")?;
                self.out.seek(SeekFrom::Start(0))?;
                self.out.write_all(&self.ftyp)?;
                self.out.flush()
            })();
            fail!(self, e);
        }
        Ok(self.out)
    }
}

/// QuickTime's `ftyp` (`qt  `), padded with a `free` box to the `len` the first one took.
fn qt_ftyp(len: usize) -> Vec<u8> {
    let mut b = b"qt  ".to_vec();
    b.extend_from_slice(&0x2005_0300u32.to_be_bytes());
    b.extend_from_slice(b"qt  ");
    let mut v = boxed(b"ftyp", &b);
    let pad = len.saturating_sub(v.len());
    if pad >= 8 {
        v.extend_from_slice(&(pad as u32).to_be_bytes());
        v.extend_from_slice(b"free");
        v.resize(len, 0);
    }
    v
}

/// Nero's chapter list: each mark's start (100 ns units) and name.
fn chpl(chapters: &[(i64, String)]) -> Vec<u8> {
    let mut b = vec![0u8; 4];
    b.push(chapters.len() as u8);
    for (us, name) in chapters {
        b.extend_from_slice(&(us.max(&0).unsigned_abs() * 10).to_be_bytes());
        b.push(name.len() as u8);
        b.extend_from_slice(name.as_bytes());
    }
    full(b"chpl", 1, 0, &b)
}

/// Seconds since 1904, as QuickTime's clocks count them.
fn now_1904() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() + 2_082_844_800)
}

/// How a finished file's header is made: the tables of each track (none: the empty ones a
/// fragmented header has) and what the file is.
struct Shape<'a> {
    /// Picture's table, then each sound track's.
    tables: Option<&'a [Table]>,
    mov: bool,
    chapters: &'a [(i64, String)],
}

/// A 32-bit field when the value fits, else the 64-bit version of the box: (version, bytes).
fn wide(n: u64) -> (u8, Vec<u8>) {
    match u32::try_from(n) {
        Ok(n) => (0, n.to_be_bytes().to_vec()),
        Err(_) => (1, n.to_be_bytes().to_vec()),
    }
}

fn moov(video: Option<&VideoTrack>, audio: &[AudioTrack], shape: &Shape<'_>) -> Vec<u8> {
    let tables = shape.tables;
    let now = now_1904();
    // Each track's length on the movie's own clock (milliseconds), with what comes before it.
    let ms = |i: usize, scale: u32| -> u64 {
        tables
            .and_then(|t| t.get(i))
            .map_or(0, |t| (t.first.unwrap_or(0).max(0) as u64 + t.total()) * 1000 / u64::from(scale))
    };
    let mut movie_ms = ms(0, VIDEO_SCALE);
    for (i, a) in audio.iter().enumerate() {
        movie_ms = movie_ms.max(ms(1 + i, a.sample_rate.max(1)));
    }
    let (v, dur) = wide(movie_ms);
    let mut mvhd = if v == 0 { Vec::new() } else { now.to_be_bytes().to_vec() };
    if v == 0 {
        mvhd.extend_from_slice(&(now as u32).to_be_bytes());
        mvhd.extend_from_slice(&(now as u32).to_be_bytes());
    } else {
        mvhd.extend_from_slice(&now.to_be_bytes());
    }
    mvhd.extend_from_slice(&1000u32.to_be_bytes());
    mvhd.extend_from_slice(&dur);
    mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    mvhd.extend_from_slice(&0x0100u16.to_be_bytes());
    mvhd.extend_from_slice(&[0; 10]);
    mvhd.extend_from_slice(&matrix());
    mvhd.extend_from_slice(&[0; 24]);
    mvhd.extend_from_slice(&(AUDIO + audio.len() as u32).to_be_bytes());
    let mut parts = vec![full(b"mvhd", v, 0, &mvhd)];
    let mut trex = Vec::new();
    if let Some(vt) = video {
        parts.push(video_trak(vt, shape, tables.and_then(|t| t.first()), ms(0, VIDEO_SCALE), now));
        trex.push(trex_box(VIDEO));
    }
    for (i, a) in audio.iter().enumerate() {
        let id = AUDIO + i as u32;
        parts.push(audio_trak(a, id, shape, tables.and_then(|t| t.get(1 + i)), ms(1 + i, a.sample_rate.max(1)), now));
        trex.push(trex_box(id));
    }
    if tables.is_some() {
        if !shape.chapters.is_empty() {
            let n = shape.chapters.len().min(MAX_CHAPTERS);
            parts.push(boxed(b"udta", &chpl(&shape.chapters[..n])));
        }
    } else {
        // Fragments need each track's defaults.
        parts.push(boxed(b"mvex", &cat(&trex)));
    }
    boxed(b"moov", &cat(&parts))
}

fn trex_box(track: u32) -> Vec<u8> {
    let mut b = track.to_be_bytes().to_vec();
    b.extend_from_slice(&1u32.to_be_bytes());
    b.extend_from_slice(&[0; 12]);
    full(b"trex", 0, 0, &b)
}

/// A track's header. Every track is enabled and in the movie: the sound tracks share alternate
/// group 1, so a player plays one of them while an editor gets them all, unmuted.
fn tkhd(track: u32, audio: bool, w: u32, h: u32, movie_ms: u64, now: u64) -> Vec<u8> {
    let (v, dur) = wide(movie_ms);
    let mut b = if v == 0 { Vec::new() } else { now.to_be_bytes().to_vec() };
    if v == 0 {
        b.extend_from_slice(&(now as u32).to_be_bytes());
        b.extend_from_slice(&(now as u32).to_be_bytes());
    } else {
        b.extend_from_slice(&now.to_be_bytes());
    }
    b.extend_from_slice(&track.to_be_bytes());
    b.extend_from_slice(&[0; 4]);
    b.extend_from_slice(&dur);
    b.extend_from_slice(&[0; 8]);
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&(if audio { 1u16 } else { 0 }).to_be_bytes());
    b.extend_from_slice(&(if audio { 0x0100u16 } else { 0 }).to_be_bytes());
    b.extend_from_slice(&[0; 2]);
    b.extend_from_slice(&matrix());
    b.extend_from_slice(&(w << 16).to_be_bytes());
    b.extend_from_slice(&(h << 16).to_be_bytes());
    // Enabled, and in the movie.
    full(b"tkhd", v, 3, &b)
}

fn mdhd(scale: u32, ticks: u64, mov: bool, now: u64) -> Vec<u8> {
    let (v, dur) = wide(ticks);
    let mut b = if v == 0 { Vec::new() } else { now.to_be_bytes().to_vec() };
    if v == 0 {
        b.extend_from_slice(&(now as u32).to_be_bytes());
        b.extend_from_slice(&(now as u32).to_be_bytes());
    } else {
        b.extend_from_slice(&now.to_be_bytes());
    }
    b.extend_from_slice(&scale.to_be_bytes());
    b.extend_from_slice(&dur);
    // The language: "und" in an MP4; a MOV has Macintosh codes, where 32767 is "unspecified".
    b.extend_from_slice(&(if mov { 0x7FFFu16 } else { 0x55C4 }).to_be_bytes());
    b.extend_from_slice(&[0; 2]);
    full(b"mdhd", v, 0, &b)
}

/// The handler: ISO's (a C string) or QuickTime's (`mhlr`, Apple, a Pascal string).
fn hdlr(kind: &[u8; 4], name: &str, mov: bool) -> Vec<u8> {
    let mut b = Vec::new();
    if mov {
        b.extend_from_slice(b"mhlr");
        b.extend_from_slice(kind);
        b.extend_from_slice(b"appl");
        b.extend_from_slice(&[0; 8]);
        b.push(name.len() as u8);
        b.extend_from_slice(name.as_bytes());
    } else {
        b.extend_from_slice(&[0; 4]);
        b.extend_from_slice(kind);
        b.extend_from_slice(&[0; 12]);
        b.extend_from_slice(name.as_bytes());
        b.push(0);
    }
    full(b"hdlr", 0, 0, &b)
}

/// The data information: where the samples are (this file), in ISO's or QuickTime's words.
fn dinf(mov: bool) -> Vec<u8> {
    let mut dref = 1u32.to_be_bytes().to_vec();
    let mut children = Vec::new();
    if mov {
        // QuickTime's data handler, then an alias reference to this same file.
        children.extend_from_slice(&full(b"alis", 0, 1, &[]));
    } else {
        children.extend_from_slice(&full(b"url ", 0, 1, &[]));
    }
    dref.extend_from_slice(&children);
    let mut parts = Vec::new();
    if mov {
        let mut h = Vec::new();
        h.extend_from_slice(b"dhlr");
        h.extend_from_slice(b"alis");
        h.extend_from_slice(b"appl");
        h.extend_from_slice(&[0; 8]);
        let name = "Alias Data Handler";
        h.push(name.len() as u8);
        h.extend_from_slice(name.as_bytes());
        // Inside `minf`, beside `dinf` (the caller places it): returned with it for QuickTime.
        parts.push(full(b"hdlr", 0, 0, &h));
    }
    parts.push(boxed(b"dinf", &full(b"dref", 0, 0, &dref)));
    cat(&parts)
}

/// An stbl with the sample entry and, in a fragmented header, the empty tables fragments need;
/// in a finished file, the track's real ones.
fn stbl(entry: Vec<u8>, table: Option<&Table>) -> Vec<u8> {
    let mut stsd = 1u32.to_be_bytes().to_vec();
    stsd.extend_from_slice(&entry);
    let mut parts = vec![full(b"stsd", 0, 0, &stsd)];
    let Some(t) = table else {
        let empty = 0u32.to_be_bytes();
        parts.extend([
            full(b"stts", 0, 0, &empty),
            full(b"stsc", 0, 0, &empty),
            full(b"stsz", 0, 0, &[0; 8]),
            full(b"stco", 0, 0, &empty),
        ]);
        return cat(&parts);
    };
    let be = |v: u32| v.to_be_bytes();
    let runs = t.runs();
    let mut stts = be(runs.len() as u32).to_vec();
    for (n, d) in &runs {
        stts.extend_from_slice(&be(*n));
        stts.extend_from_slice(&be(*d));
    }
    parts.push(full(b"stts", 0, 0, &stts));
    // Composition offsets (video), only when some picture isn't shown at its decode time.
    if t.cts.iter().any(|c| *c != 0) {
        let mut runs: Vec<(u32, i32)> = Vec::new();
        for c in &t.cts {
            match runs.last_mut() {
                Some(r) if r.1 == *c => r.0 += 1,
                _ => runs.push((1, *c)),
            }
        }
        let mut ctts = be(runs.len() as u32).to_vec();
        for (n, c) in runs {
            ctts.extend_from_slice(&be(n));
            ctts.extend_from_slice(&c.to_be_bytes());
        }
        parts.push(full(b"ctts", 1, 0, &ctts));
    }
    // Sync samples (video), unless every one is.
    if !t.keys.is_empty() && t.keys.len() < t.samples() {
        let mut stss = be(t.keys.len() as u32).to_vec();
        for k in &t.keys {
            stss.extend_from_slice(&be(*k));
        }
        parts.push(full(b"stss", 0, 0, &stss));
    }
    // Samples per chunk, in runs of the same count.
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for (i, (_, n)) in t.chunks.iter().enumerate() {
        if runs.last().is_none_or(|r| r.1 != *n) {
            runs.push((i as u32 + 1, *n));
        }
    }
    let mut stsc = be(runs.len() as u32).to_vec();
    for (first, n) in runs {
        stsc.extend_from_slice(&be(first));
        stsc.extend_from_slice(&be(n));
        stsc.extend_from_slice(&be(1));
    }
    parts.push(full(b"stsc", 0, 0, &stsc));
    let mut stsz = be(0).to_vec();
    stsz.extend_from_slice(&be(t.sizes.len() as u32));
    for z in &t.sizes {
        stsz.extend_from_slice(&be(*z));
    }
    parts.push(full(b"stsz", 0, 0, &stsz));
    // Chunk offsets: 32-bit when they all fit, else 64.
    if t.chunks.iter().all(|(at, _)| u32::try_from(*at).is_ok()) {
        let mut co = be(t.chunks.len() as u32).to_vec();
        for (at, _) in &t.chunks {
            co.extend_from_slice(&be(*at as u32));
        }
        parts.push(full(b"stco", 0, 0, &co));
    } else {
        let mut co = be(t.chunks.len() as u32).to_vec();
        for (at, _) in &t.chunks {
            co.extend_from_slice(&at.to_be_bytes());
        }
        parts.push(full(b"co64", 0, 0, &co));
    }
    cat(&parts)
}

/// An edit list that starts a track late: an empty edit for `delay_ms` (on the movie's clock),
/// then the whole media.
fn edts(delay_ms: u64, media_ms: u64) -> Vec<u8> {
    let mut b = 2u32.to_be_bytes().to_vec();
    b.extend_from_slice(&(delay_ms as u32).to_be_bytes());
    b.extend_from_slice(&(-1i32).to_be_bytes());
    b.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    b.extend_from_slice(&(media_ms as u32).to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    boxed(b"edts", &full(b"elst", 0, 0, &b))
}

fn video_trak(v: &VideoTrack, shape: &Shape<'_>, table: Option<&Table>, movie_ms: u64, now: u64) -> Vec<u8> {
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
    // Square pixels.
    e.extend_from_slice(&boxed(b"pasp", &[0, 0, 0, 1, 0, 0, 0, 1]));
    let minf = boxed(
        b"minf",
        &cat(&[full(b"vmhd", 0, 1, &[0; 8]), dinf(shape.mov), boxed(b"stbl", &stbl(boxed(entry, &e), table))]),
    );
    let media_ticks = table.map_or(0, Table::total);
    let mdia = boxed(
        b"mdia",
        &cat(&[mdhd(VIDEO_SCALE, media_ticks, shape.mov, now), hdlr(b"vide", "Ferrocast video", shape.mov), minf]),
    );
    boxed(b"trak", &cat(&[tkhd(VIDEO, false, v.width, v.height, movie_ms, now), mdia]))
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

fn audio_trak(a: &AudioTrack, id: u32, shape: &Shape<'_>, table: Option<&Table>, movie_ms: u64, now: u64) -> Vec<u8> {
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
    let mut es = vec![0, id as u8, 0];
    es.extend_from_slice(&descriptor(0x04, &dc));
    es.extend_from_slice(&descriptor(0x06, &[0x02]));
    e.extend_from_slice(&full(b"esds", 0, 0, &descriptor(0x03, &es)));
    let minf = boxed(
        b"minf",
        &cat(&[full(b"smhd", 0, 0, &[0; 4]), dinf(shape.mov), boxed(b"stbl", &stbl(boxed(b"mp4a", &e), table))]),
    );
    let media_ticks = table.map_or(0, Table::total);
    let mdia = boxed(
        b"mdia",
        &cat(&[
            mdhd(a.sample_rate, media_ticks, shape.mov, now),
            hdlr(b"soun", if a.name.is_empty() { "Ferrocast sound" } else { &a.name }, shape.mov),
            minf,
        ]),
    );
    // A sound that started after the picture does so in the finished file too.
    let delay_ms =
        table.and_then(|t| t.first).filter(|f| *f > 0).map(|f| f as u64 * 1000 / u64::from(a.sample_rate.max(1)));
    let mut parts = vec![tkhd(id, true, 0, 0, movie_ms, now)];
    if let Some(d) = delay_ms.filter(|d| *d > 0) {
        parts.push(edts(d, movie_ms.saturating_sub(d)));
    }
    parts.push(mdia);
    boxed(b"trak", &cat(&parts))
}

/// A finished file is a regular MP4 (M7-03): read back with a small box parser, no player needed.
#[cfg(test)]
mod finished_tests {
    use std::io::Cursor;

    use super::*;

    /// A box: its type, and where its body is in the file.
    #[derive(Debug, Clone)]
    struct B {
        kind: [u8; 4],
        at: usize,
        end: usize,
    }

    fn boxes_in(b: &[u8], from: usize, to: usize) -> Vec<B> {
        let (mut at, mut out) = (from, Vec::new());
        while at + 8 <= to {
            let size = u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as usize;
            assert!(size >= 8 && at + size <= to, "a box that is cut or wrong at {at}: size {size}");
            out.push(B { kind: [b[at + 4], b[at + 5], b[at + 6], b[at + 7]], at: at + 8, end: at + size });
            at += size;
        }
        assert_eq!(at, to, "the boxes end exactly where their parent does");
        out
    }

    fn find<'a>(list: &'a [B], kind: &[u8; 4]) -> Option<&'a B> {
        list.iter().find(|b| &b.kind == kind)
    }

    fn path(b: &[u8], root: &[B], names: &[&[u8; 4]]) -> Option<B> {
        let mut cur: Vec<B> = root.to_vec();
        let mut found = None;
        for (i, n) in names.iter().enumerate() {
            let x = find(&cur, n)?.clone();
            // Only a container has boxes inside.
            if i + 1 < names.len() {
                cur = boxes_in(b, x.at, x.end);
            }
            found = Some(x);
        }
        found
    }

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }

    fn tracks() -> (VideoTrack, AudioTrack) {
        (
            VideoTrack {
                codec: VideoCodec::H264,
                width: 64,
                height: 64,
                config: vec![1, 100, 0, 31, 255, 225, 0, 0, 1, 0, 0],
            },
            AudioTrack {
                sample_rate: 48000,
                channels: 2,
                config: vec![0x11, 0x90],
                bitrate_kbps: 160,
                name: String::new(),
            },
        )
    }

    /// The streamer's names for the audio tracks show in the file (the handler's name); a track
    /// without one keeps "Ferrocast sound".
    #[test]
    fn audio_tracks_carry_the_names_they_were_given() {
        let (v, a) = tracks();
        let named = |n: &str| AudioTrack { name: n.into(), ..a.clone() };
        let mut w = Mp4Writer::with_tracks(Cursor::new(Vec::new()), Some(v), vec![named("Mic"), named("Game"), a])
            .unwrap_or_else(|e| panic!("{e}"));
        for i in 0..3i64 {
            w.video(&VideoPacket { pts_us: i * 33_333, dts_us: i * 33_333, key: i == 0, data: vec![1; 40] })
                .unwrap_or_else(|e| panic!("{e}"));
            for t in 0..3 {
                w.audio_on(t, &AudioPacket { pts_us: i * 21_333, data: vec![9; 20] }).unwrap_or_else(|e| panic!("{e}"));
            }
        }
        let bytes = w.finish().unwrap_or_else(|e| panic!("{e}")).into_inner();
        let has = |what: &[u8]| bytes.windows(what.len()).any(|w| w == what);
        assert!(has(b"Mic") && has(b"Game"), "the names are in the file");
        assert_eq!(
            bytes.windows(15).filter(|w| *w == b"Ferrocast sound").count(),
            1,
            "the unnamed one keeps the default"
        );
    }

    /// 150 frames (a keyframe each second), and three sound tracks, three chapters.
    fn recorded(flavor: Flavor) -> (Mp4Writer<Cursor<Vec<u8>>>, Vec<Vec<u8>>) {
        let (v, a) = tracks();
        let mut w = Mp4Writer::with_flavor(Cursor::new(Vec::new()), Some(v), vec![a.clone(), a.clone(), a], flavor)
            .unwrap_or_else(|e| panic!("{e}"));
        let mut frames = Vec::new();
        let mut next_audio = 0i64;
        for i in 0..150i64 {
            let pts = i * 33_333;
            let data: Vec<u8> = (0..400u32).map(|n| (n as i64 + i) as u8).collect();
            frames.push(data.clone());
            w.video(&VideoPacket { pts_us: pts, dts_us: pts, key: i % 30 == 0, data })
                .unwrap_or_else(|e| panic!("{e}"));
            while next_audio <= pts {
                for t in 0..3 {
                    w.audio_on(t, &AudioPacket { pts_us: next_audio, data: vec![t as u8 + 1; 30] })
                        .unwrap_or_else(|e| panic!("{e}"));
                }
                next_audio += 21_333;
            }
            match i {
                0 => assert!(w.chapter("Intro")),
                60 => assert!(w.chapter("Game")),
                120 => assert!(w.chapter("")),
                _ => {}
            }
        }
        (w, frames)
    }

    #[test]
    fn a_finished_file_is_a_regular_mp4_with_its_samples_where_the_tables_say() {
        let (w, frames) = recorded(Flavor::Mp4);
        let bytes = w.finalize().unwrap_or_else(|e| panic!("{}", e.error)).into_inner();
        let top = boxes_in(&bytes, 0, bytes.len());
        // A header at the end, and no fragment headers or fragmented header left.
        assert_eq!(top.last().map(|b| b.kind), Some(*b"moov"), "{top:?}");
        assert!(top.iter().all(|b| &b.kind != b"moof"), "every moof became a free box");
        assert_eq!(top.iter().filter(|b| &b.kind == b"moov").count(), 1, "one header");
        assert_eq!(&top[0].kind, b"ftyp");
        assert_eq!(&top[1].kind, b"free", "the first header is a free box now");
        let moov = top.last().cloned().unwrap_or_else(|| panic!("a moov"));
        let inside = boxes_in(&bytes, moov.at, moov.end);
        assert!(find(&inside, b"mvex").is_none(), "no fragmented-file header");
        // Real durations: 150 frames at 30 fps is 5 s; the sound tracks are as long.
        let mvhd = find(&inside, b"mvhd").unwrap_or_else(|| panic!("mvhd"));
        let movie_ms = u32_at(&bytes, mvhd.at + 16);
        assert!((4900..=5100).contains(&movie_ms), "{movie_ms} ms");
        // Four tracks, all enabled and in the movie (flags 3), sound in one alternate group.
        let traks: Vec<&B> = inside.iter().filter(|b| &b.kind == b"trak").collect();
        assert_eq!(traks.len(), 4);
        for (i, t) in traks.iter().enumerate() {
            let parts = boxes_in(&bytes, t.at, t.end);
            let tkhd = find(&parts, b"tkhd").unwrap_or_else(|| panic!("tkhd"));
            let flags = u32_at(&bytes, tkhd.at) & 0x00FF_FFFF;
            assert_eq!(flags, 3, "track {} is enabled and in the movie", i + 1);
            if i > 0 {
                let alt = u16::from_be_bytes([bytes[tkhd.at + 34], bytes[tkhd.at + 35]]);
                assert_eq!(alt, 1, "the sound tracks are alternates of each other");
            }
        }
        // The picture's tables: 150 samples, 5 keyframes, 5 chunks (a fragment each), and the
        // first sample of each chunk is the very bytes that were written.
        let video = boxes_in(&bytes, traks[0].at, traks[0].end);
        let stbl = |parts: &[B]| {
            let mdia = find(parts, b"mdia").unwrap_or_else(|| panic!("mdia")).clone();
            let minf = find(&boxes_in(&bytes, mdia.at, mdia.end), b"minf").unwrap_or_else(|| panic!("minf")).clone();
            let stbl = find(&boxes_in(&bytes, minf.at, minf.end), b"stbl").unwrap_or_else(|| panic!("stbl")).clone();
            boxes_in(&bytes, stbl.at, stbl.end)
        };
        let st = stbl(&video);
        let stsz = find(&st, b"stsz").unwrap_or_else(|| panic!("stsz"));
        assert_eq!(u32_at(&bytes, stsz.at + 8), 150, "samples");
        let stss = find(&st, b"stss").unwrap_or_else(|| panic!("stss"));
        assert_eq!(u32_at(&bytes, stss.at + 4), 5, "keyframes");
        assert_eq!(u32_at(&bytes, stss.at + 8), 1, "the first sample is one");
        let stco = find(&st, b"stco").unwrap_or_else(|| panic!("stco"));
        let chunks = u32_at(&bytes, stco.at + 4) as usize;
        assert_eq!(chunks, 5);
        let stsc = find(&st, b"stsc").unwrap_or_else(|| panic!("stsc"));
        assert_eq!((u32_at(&bytes, stsc.at + 4), u32_at(&bytes, stsc.at + 12)), (1, 30), "one run: 30 samples a chunk");
        for c in 0..chunks {
            let at = u32_at(&bytes, stco.at + 8 + 4 * c) as usize;
            let want = &frames[c * 30];
            assert_eq!(&bytes[at..at + want.len()], &want[..], "chunk {c} starts with its first frame");
        }
        // Sound: every track has every frame of its own (the byte they were filled with).
        for (t, trak) in traks.iter().enumerate().skip(1) {
            let st = stbl(&boxes_in(&bytes, trak.at, trak.end));
            let stsz = find(&st, b"stsz").unwrap_or_else(|| panic!("stsz"));
            assert!(u32_at(&bytes, stsz.at + 8) > 200, "track {t} has its sound");
            let stco = find(&st, b"stco").unwrap_or_else(|| panic!("stco"));
            let at = u32_at(&bytes, stco.at + 8) as usize;
            assert_eq!(bytes[at], t as u8, "track {t}'s first sample is its own");
        }
        // The chapters are in the header.
        let udta = find(&inside, b"udta").unwrap_or_else(|| panic!("udta"));
        let chpl = boxes_in(&bytes, udta.at, udta.end);
        let chpl = find(&chpl, b"chpl").unwrap_or_else(|| panic!("chpl"));
        assert_eq!(bytes[chpl.at + 8], 3, "three chapters");
    }

    #[test]
    fn a_crash_before_the_end_leaves_the_fragmented_file_that_plays() {
        let (w, _) = recorded(Flavor::Mp4);
        // The process dies: no finalize. The bytes are what the disk has.
        let (out, whole) = w.abandon();
        let bytes = out.into_inner();
        assert!(whole as usize <= bytes.len());
        let top = boxes_in(&bytes, 0, whole as usize);
        assert!(top.iter().any(|b| &b.kind == b"moof"), "fragments");
        let moov = find(&top, b"moov").unwrap_or_else(|| panic!("the header is first"));
        let inside = boxes_in(&bytes, moov.at, moov.end);
        assert!(find(&inside, b"mvex").is_some(), "the fragmented header");
        assert_eq!(&top[0].kind, b"ftyp");
        assert_eq!(&top.last().map(|b| b.kind), &Some(*b"mdat"));
    }

    /// A file that takes `room` bytes in all and then says the disk is full, and can seek.
    #[derive(Debug)]
    struct Full {
        file: Cursor<Vec<u8>>,
        room: usize,
        /// A write that starts here fails (the MOV's new `ftyp`, the last step of finishing).
        fail_at: Option<u64>,
    }

    impl Write for Full {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.fail_at == Some(self.file.position()) {
                return Err(io::Error::from(io::ErrorKind::StorageFull));
            }
            let end = self.file.position() as usize + buf.len();
            if end > self.room.max(self.file.get_ref().len()) && end > self.room {
                return Err(io::Error::from(io::ErrorKind::StorageFull));
            }
            self.file.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Seek for Full {
        fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
            self.file.seek(to)
        }
    }

    #[test]
    fn a_disk_that_fills_while_finishing_leaves_the_file_as_the_fragmented_one_it_was() {
        let (v, a) = tracks();
        let mut w = Mp4Writer::with_tracks(
            Full { file: Cursor::new(Vec::new()), room: 1 << 30, fail_at: None },
            Some(v),
            vec![a],
        )
        .unwrap_or_else(|e| panic!("{e}"));
        for i in 0..90i64 {
            let pts = i * 33_333;
            w.video(&VideoPacket { pts_us: pts, dts_us: pts, key: i % 30 == 0, data: vec![i as u8; 500] })
                .unwrap_or_else(|e| panic!("{e}"));
        }
        // Everything but the last fragment is out; room for that one, and nothing more.
        let have = w.out.file.get_ref().len();
        w.out.room = have + 16_000;
        let failed = w.finalize().expect_err("the header does not fit");
        assert_eq!(failed.error.kind(), io::ErrorKind::StorageFull);
        let (mut file, whole) = (failed.out.file.into_inner(), failed.whole as usize);
        assert!(whole <= file.len());
        file.truncate(whole);
        let top = boxes_in(&file, 0, file.len());
        assert!(top.iter().all(|b| &b.kind != b"free" || b.end - b.at + 8 == ROOM || b.end - b.at < 100));
        assert_eq!(top.iter().filter(|b| &b.kind == b"moov").count(), 1, "the fragmented header, only");
        assert!(top.iter().filter(|b| &b.kind == b"moof").count() >= 3, "and all the fragments, still moof");
    }

    #[test]
    fn a_failure_at_the_very_last_step_puts_everything_back() {
        // The last step of finishing a MOV writes its new ftyp at 0: it fails, after the new
        // header, the free boxes and the first header's flip were all written.
        let (v, a) = tracks();
        let mut w = Mp4Writer::with_flavor(
            Full { file: Cursor::new(Vec::new()), room: 1 << 30, fail_at: None },
            Some(v),
            vec![a],
            Flavor::Mov,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        for i in 0..90i64 {
            let pts = i * 33_333;
            w.video(&VideoPacket { pts_us: pts, dts_us: pts, key: i % 30 == 0, data: vec![i as u8; 500] })
                .unwrap_or_else(|e| panic!("{e}"));
        }
        w.out.fail_at = Some(0);
        let failed = w.finalize().expect_err("the ftyp can't be written");
        let (mut file, whole) = (failed.out.file.into_inner(), failed.whole as usize);
        file.truncate(whole);
        let top = boxes_in(&file, 0, file.len());
        assert_eq!(&file[8..12], b"iso6", "the first ftyp is as it was");
        assert_eq!(top.iter().filter(|b| &b.kind == b"moov").count(), 1, "the first header is a header again");
        let moov = find(&top, b"moov").unwrap_or_else(|| panic!("moov"));
        assert!(find(&boxes_in(&file, moov.at, moov.end), b"mvex").is_some(), "the fragmented one");
        assert_eq!(top.iter().filter(|b| &b.kind == b"moof").count(), 3, "every fragment header is back");
    }

    #[test]
    fn a_mov_gets_quicktimes_brand_and_handlers() {
        let (w, _) = recorded(Flavor::Mov);
        let bytes = w.finalize().unwrap_or_else(|e| panic!("{}", e.error)).into_inner();
        assert_eq!(&bytes[4..12], b"ftypqt  ", "the QuickTime brand");
        let top = boxes_in(&bytes, 0, bytes.len());
        assert_eq!(&top[1].kind, b"free", "padded to the size the first ftyp had");
        let moov = top.last().cloned().unwrap_or_else(|| panic!("moov"));
        let trak = find(&boxes_in(&bytes, moov.at, moov.end), b"trak").cloned().unwrap_or_else(|| panic!("trak"));
        let mdia = find(&boxes_in(&bytes, trak.at, trak.end), b"mdia").cloned().unwrap_or_else(|| panic!("mdia"));
        let hdlr = find(&boxes_in(&bytes, mdia.at, mdia.end), b"hdlr").cloned().unwrap_or_else(|| panic!("hdlr"));
        assert_eq!(&bytes[hdlr.at + 4..hdlr.at + 8], b"mhlr", "QuickTime's media handler");
        assert_eq!(&bytes[hdlr.at + 12..hdlr.at + 16], b"appl");
        let mdhd = find(&boxes_in(&bytes, mdia.at, mdia.end), b"mdhd").cloned().unwrap_or_else(|| panic!("mdhd"));
        assert_eq!(u16::from_be_bytes([bytes[mdhd.at + 20], bytes[mdhd.at + 21]]), 0x7FFF, "language: unspecified");
    }

    #[test]
    fn a_pause_cut_keeps_the_fragmented_files_times_in_the_tables() {
        // Fragment two starts 33 ms before the first one's frames end (a cut): the tables follow
        // the fragments' own times, not a running sum, so sound and picture stay together.
        let (v, a) = tracks();
        let mut w = Mp4Writer::new(Cursor::new(Vec::new()), Some(v), Some(a)).unwrap_or_else(|e| panic!("{e}"));
        let mut t = 0i64;
        for i in 0..60i64 {
            let pts = t;
            w.video(&VideoPacket { pts_us: pts, dts_us: pts, key: i % 30 == 0, data: vec![1; 100] })
                .unwrap_or_else(|e| panic!("{e}"));
            // The second keyframe lands 33 ms before where the 30th frame would end.
            t += if i == 29 { 0 } else { 33_333 };
        }
        let bytes = w.finalize().unwrap_or_else(|e| panic!("{}", e.error)).into_inner();
        let top = boxes_in(&bytes, 0, bytes.len());
        let moov = top.last().cloned().unwrap_or_else(|| panic!("moov"));
        let trak = find(&boxes_in(&bytes, moov.at, moov.end), b"trak").cloned().unwrap_or_else(|| panic!("trak"));
        let mdhd =
            path(&bytes, &boxes_in(&bytes, trak.at, trak.end), &[b"mdia", b"mdhd"]).unwrap_or_else(|| panic!("mdhd"));
        // The media ends where the last frame's own time says: 59 frames after the start of the
        // 29 + ... 1 s (30 x 33333 us minus the overlap): well under what a running sum says.
        let dur = u32_at(&bytes, mdhd.at + 16);
        let sum_us = 60 * 33_333i64;
        assert!(i64::from(dur) * 1_000_000 / 90_000 < sum_us, "{dur} ticks is under the running sum");
    }

    /// A reordered stream as VideoToolbox gives it with B-frames: decode order I P B B P B B (a
    /// pyramid two frames deep), decode times shifted back three frames so none passes its
    /// picture's time. Pictures are `n` frames of 33 333 us; returns packets and each one's
    /// display index.
    fn reordered(n: i64) -> Vec<(VideoPacket, i64)> {
        let f = 33_333;
        // Display order 0 1 2 3 ...; decode order inside each group of three: 2 0 1 (P first).
        let mut order = vec![0];
        let mut g = 1;
        while order.len() < n as usize {
            for k in [g + 2, g, g + 1] {
                if k < n {
                    order.push(k);
                }
            }
            g += 3;
        }
        order
            .iter()
            .enumerate()
            .map(|(k, &shown)| {
                let p = VideoPacket {
                    pts_us: shown * f,
                    dts_us: (k as i64 - 3) * f + 2 * f,
                    key: k % 30 == 0,
                    data: vec![k as u8; 300],
                };
                assert!(p.dts_us <= p.pts_us);
                (p, shown)
            })
            .collect()
    }

    /// The finished file's picture tables: per sample (decode ticks, composition ticks, sync).
    fn video_samples(bytes: &[u8]) -> Vec<(i64, i64, bool)> {
        let top = boxes_in(bytes, 0, bytes.len());
        let moov = find(&top, b"moov").unwrap_or_else(|| panic!("moov")).clone();
        let trak = find(&boxes_in(bytes, moov.at, moov.end), b"trak").unwrap_or_else(|| panic!("trak")).clone();
        let mdia = find(&boxes_in(bytes, trak.at, trak.end), b"mdia").unwrap_or_else(|| panic!("mdia")).clone();
        let minf = find(&boxes_in(bytes, mdia.at, mdia.end), b"minf").unwrap_or_else(|| panic!("minf")).clone();
        let stbl = find(&boxes_in(bytes, minf.at, minf.end), b"stbl").unwrap_or_else(|| panic!("stbl")).clone();
        let st = boxes_in(bytes, stbl.at, stbl.end);
        let stts = find(&st, b"stts").unwrap_or_else(|| panic!("stts"));
        let mut dts = Vec::new();
        let (mut t, runs) = (0i64, u32_at(bytes, stts.at + 4) as usize);
        for r in 0..runs {
            let (n, d) = (u32_at(bytes, stts.at + 8 + 8 * r), u32_at(bytes, stts.at + 12 + 8 * r));
            for _ in 0..n {
                dts.push(t);
                t += i64::from(d);
            }
        }
        let mut cts = vec![0i64; dts.len()];
        if let Some(c) = find(&st, b"ctts") {
            assert_eq!(bytes[c.at], 1, "version 1: signed offsets");
            let (mut i, runs) = (0, u32_at(bytes, c.at + 4) as usize);
            for r in 0..runs {
                let n = u32_at(bytes, c.at + 8 + 8 * r) as usize;
                let off = u32_at(bytes, c.at + 12 + 8 * r) as i32;
                for _ in 0..n {
                    cts[i] = i64::from(off);
                    i += 1;
                }
            }
            assert_eq!(i, dts.len(), "an offset for every sample");
        }
        let keys: Vec<u32> = find(&st, b"stss")
            .map(|k| (0..u32_at(bytes, k.at + 4) as usize).map(|i| u32_at(bytes, k.at + 8 + 4 * i)).collect())
            .unwrap_or_default();
        dts.iter().zip(&cts).enumerate().map(|(i, (d, c))| (*d, *c, keys.contains(&(i as u32 + 1)))).collect()
    }

    fn written_reordered(frames: &[(VideoPacket, i64)]) -> Mp4Writer<Cursor<Vec<u8>>> {
        let (v, a) = tracks();
        let mut w = Mp4Writer::new(Cursor::new(Vec::new()), Some(v), Some(a)).unwrap_or_else(|e| panic!("{e}"));
        let mut next_audio = 0i64;
        for (p, _) in frames {
            // Sound in time order, by decode time as the pipeline interleaves it.
            while next_audio <= p.dts_us {
                w.audio(&AudioPacket { pts_us: next_audio, data: vec![9; 30] }).unwrap_or_else(|e| panic!("{e}"));
                next_audio += 21_333;
            }
            w.video(p).unwrap_or_else(|e| panic!("{e}"));
        }
        w
    }

    /// B-frames in a finished file: decode times rise from 0, composition offsets give back each
    /// picture's own time, and the pictures show at the same clock the sound is on.
    #[test]
    fn a_finished_file_keeps_each_pictures_decode_and_display_times_with_b_frames() {
        let frames = reordered(90);
        let bytes = written_reordered(&frames).finalize().unwrap_or_else(|e| panic!("{}", e.error)).into_inner();
        let got = video_samples(&bytes);
        assert_eq!(got.len(), 90);
        assert_eq!(got[0].0, 0, "the file starts at the first decode time");
        assert!(got.windows(2).all(|w| w[1].0 > w[0].0), "decode times rise");
        let start = frames[0].0.dts_us;
        for (g, (p, _)) in got.iter().zip(&frames) {
            let want_pts = (p.pts_us - start) * 90_000 / 1_000_000;
            assert!((g.0 + g.1 - want_pts).abs() <= 1, "display time {} vs {want_pts}", g.0 + g.1);
            assert!(g.1 >= 0, "never displayed before decoded");
        }
        assert!(got.iter().any(|g| g.1 > 0), "some pictures are reordered");
        assert_eq!(got.iter().filter(|g| g.2).count(), 3, "keyframes are the sync samples");
        // A/V: the first picture shows at the clock's `first pts - start` ticks, as the sound's
        // timeline counts (sound from pts 0 starts that far in, by an empty edit).
        let first_shown = got.iter().map(|g| g.0 + g.1).min().unwrap_or(0);
        let want_ms = (frames.iter().map(|f| f.0.pts_us).min().unwrap_or(0) - start) / 1000;
        assert!((first_shown * 1000 / 90_000 - want_ms).abs() <= 33, "{first_shown} ticks vs {want_ms} ms");
        // The sound's empty edit (where it starts on the movie's clock) is the first picture's time.
        let top = boxes_in(&bytes, 0, bytes.len());
        let moov = find(&top, b"moov").unwrap_or_else(|| panic!("moov")).clone();
        let inside = boxes_in(&bytes, moov.at, moov.end);
        let traks: Vec<&B> = inside.iter().filter(|b| &b.kind == b"trak").collect();
        let parts = boxes_in(&bytes, traks[1].at, traks[1].end);
        let edts = find(&parts, b"edts").unwrap_or_else(|| panic!("the sound starts late"));
        let elst = find(&boxes_in(&bytes, edts.at, edts.end), b"elst").unwrap_or_else(|| panic!("elst")).clone();
        let delay_ms = i64::from(u32_at(&bytes, elst.at + 8));
        assert!(
            (delay_ms - first_shown * 1000 / 90_000).abs() <= 33,
            "sound starts at {delay_ms} ms, picture at {first_shown} ticks"
        );
    }

    /// The same in the fragmented file a crash leaves: each fragment's trun carries the offsets.
    #[test]
    fn a_fragmented_file_carries_composition_offsets_in_each_run() {
        let frames = reordered(90);
        let (out, whole) = written_reordered(&frames).abandon();
        let bytes = out.into_inner();
        let top = boxes_in(&bytes, 0, whole as usize);
        let mut offsets = Vec::new();
        for moof in top.iter().filter(|b| &b.kind == b"moof") {
            let traf = boxes_in(&bytes, moof.at, moof.end);
            for t in traf.iter().filter(|b| &b.kind == b"traf") {
                let parts = boxes_in(&bytes, t.at, t.end);
                let tfhd = find(&parts, b"tfhd").unwrap_or_else(|| panic!("tfhd"));
                if u32_at(&bytes, tfhd.at + 4) != VIDEO {
                    continue;
                }
                let trun = find(&parts, b"trun").unwrap_or_else(|| panic!("trun"));
                assert_eq!(bytes[trun.at], 1, "version 1: signed offsets");
                let n = u32_at(&bytes, trun.at + 4) as usize;
                for i in 0..n {
                    offsets.push(u32_at(&bytes, trun.at + 12 + 16 * i + 12) as i32);
                }
            }
        }
        // The last group isn't written until the next keyframe: what is, is in order.
        assert!(offsets.len() >= 60, "{} samples", offsets.len());
        let want: Vec<i32> = frames
            .iter()
            .take(offsets.len())
            .map(|(p, _)| ((p.pts_us - p.dts_us) * 90_000 / 1_000_000) as i32)
            .collect();
        for (o, w) in offsets.iter().zip(&want) {
            assert!((o - w).abs() <= 1, "{o} vs {w}");
        }
        assert!(offsets.iter().any(|o| *o > 0));
    }

    #[test]
    fn fragment_v_next_never_overlaps_when_base_advances() {
        let (v, a) = tracks();
        let mut w = Mp4Writer::new(Cursor::new(Vec::new()), Some(v), Some(a)).unwrap_or_else(|e| panic!("{e}"));
        // Fragment 1: keyframe at 0, frame at 33_333 (last_dur becomes 3000 ticks = 33_333 us).
        // Suppose frame 2 is a keyframe slightly early or base was pushed forward:
        // e.g. force v_next ahead by pushing a single-frame fragment (dts=0 -> v_next=3000),
        // then keyframe at 10_000 us (900 ticks) and 43_333 us (3900 ticks), then keyframe at 100_000 us.
        w.video(&VideoPacket { pts_us: 0, dts_us: 0, key: true, data: vec![1; 20] }).unwrap_or_else(|e| panic!("{e}"));
        w.video(&VideoPacket { pts_us: 10_000, dts_us: 10_000, key: true, data: vec![1; 20] })
            .unwrap_or_else(|e| panic!("{e}"));
        w.video(&VideoPacket { pts_us: 43_333, dts_us: 43_333, key: false, data: vec![1; 20] })
            .unwrap_or_else(|e| panic!("{e}"));
        w.video(&VideoPacket { pts_us: 75_000, dts_us: 75_000, key: true, data: vec![1; 20] })
            .unwrap_or_else(|e| panic!("{e}"));
        w.video(&VideoPacket { pts_us: 110_000, dts_us: 110_000, key: true, data: vec![1; 20] })
            .unwrap_or_else(|e| panic!("{e}"));
        let (out, whole) = w.abandon();
        let bytes = out.into_inner();
        let top = boxes_in(&bytes, 0, whole as usize);
        let mut ranges = Vec::new();
        for moof in top.iter().filter(|b| &b.kind == b"moof") {
            let traf = boxes_in(&bytes, moof.at, moof.end);
            for t in traf.iter().filter(|b| &b.kind == b"traf") {
                let parts = boxes_in(&bytes, t.at, t.end);
                let tfhd = find(&parts, b"tfhd").unwrap_or_else(|| panic!("tfhd"));
                if u32_at(&bytes, tfhd.at + 4) != VIDEO {
                    continue;
                }
                let tfdt = find(&parts, b"tfdt").unwrap_or_else(|| panic!("tfdt"));
                let base = u64::from_be_bytes(bytes[tfdt.at + 4..tfdt.at + 12].try_into().unwrap_or([0; 8]));
                let trun = find(&parts, b"trun").unwrap_or_else(|| panic!("trun"));
                let n = u32_at(&bytes, trun.at + 4) as usize;
                let mut sum_dur = 0u64;
                for i in 0..n {
                    sum_dur += u64::from(u32_at(&bytes, trun.at + 12 + 16 * i));
                }
                ranges.push((base, base + sum_dur));
            }
        }
        assert!(ranges.len() >= 2, "{ranges:?}");
        for w in ranges.windows(2) {
            assert!(w[1].0 >= w[0].1, "fragment tfdt overlapped: prev {:?} vs next {:?}", w[0], w[1]);
        }
    }
}
