//! One output: frames and sound in, on one clock (nanoseconds, libobs's or ours), encoded once,
//! then sent as FLV tags (to Go live's hub, which streams to every platform) and recorded to a
//! crash-safe MP4, both from the same packets. Time 0 is the first frame; sound from before it
//! is left out, so picture and sound start together.

use std::collections::VecDeque;
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use crate::AvError;
use crate::audio::{AudioConfig, AudioEncoder, AudioPacket};
use crate::flv;
use crate::mkv::MkvWriter;
use crate::mp4::{AudioTrack, Mp4Writer, VideoTrack};
use crate::video::{Frame, VideoConfig, VideoEncoder, VideoPacket};

/// An FLV tag body for the stream, with its time in milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tag {
    Video { ms: u32, body: Vec<u8> },
    Audio { ms: u32, body: Vec<u8> },
}

#[derive(Debug)]
pub struct Pipeline {
    video: VideoEncoder,
    audio: AudioEncoder,
    /// The first frame's time: 0 for everything after.
    start_ns: Option<u64>,
    /// Where the sound's first sample sits on the output's clock.
    audio_offset_us: Option<i64>,
    sent_video_header: bool,
    sent_audio_header: bool,
    recording: Option<Recording>,
    /// More sound tracks, recorded only (OBS's tracks 2-6): each its encoder, and where its
    /// first sample sits.
    extra: Vec<(AudioEncoder, Option<i64>)>,
    /// Paused since this time (ns): nothing is encoded until it resumes.
    paused_at: Option<u64>,
    /// Time spent paused so far, taken off every later frame's time (no gap in the file).
    paused_ns: u64,
    /// The next frame is made a keyframe (after a pause, or to start a split file on).
    key_next: bool,
    /// A new file to go on in at the next keyframe ("Split recording").
    split_to: Option<(File, bool)>,
    /// The replay buffer's memory, when this output is one.
    replay: Option<Replay>,
}

/// The last seconds of encoded picture and sound, kept from a keyframe on (OBS's replay buffer).
#[derive(Debug)]
struct Replay {
    keep_us: i64,
    max_bytes: usize,
    video: VecDeque<VideoPacket>,
    /// Sound with its track (0 is the first).
    audio: VecDeque<(usize, AudioPacket)>,
    bytes: usize,
}

impl Replay {
    fn video(&mut self, p: &VideoPacket) {
        self.bytes += p.data.len();
        self.video.push_back(p.clone());
        let newest = p.pts_us;
        // Drop whole keyframe runs from the front while what's left still covers `keep_us`
        // (or while it's over the size limit and another keyframe follows).
        loop {
            let next_key = self.video.iter().skip(1).position(|v| v.key).map(|i| i + 1);
            let Some(k) = next_key else { break };
            let long = newest - self.video[k].pts_us >= self.keep_us;
            let big = self.max_bytes > 0 && self.bytes > self.max_bytes;
            if !(long || big) {
                break;
            }
            for v in self.video.drain(..k) {
                self.bytes -= v.data.len();
            }
        }
        let first = self.video.front().map_or(0, |v| v.dts_us.min(v.pts_us));
        while self.audio.front().is_some_and(|(_, a)| a.pts_us < first) {
            if let Some((_, a)) = self.audio.pop_front() {
                self.bytes -= a.data.len();
            }
        }
    }

    fn audio(&mut self, track: usize, p: &AudioPacket) {
        if self.video.is_empty() {
            return;
        }
        self.bytes += p.data.len();
        self.audio.push_back((track, p.clone()));
    }
}

/// What a replay buffer held when asked to save: written to a file by `write`, off the
/// engine's thread.
#[derive(Debug)]
pub struct ReplayClip {
    video: VideoTrack,
    audio: AudioTrack,
    tracks: usize,
    packets: Vec<VideoPacket>,
    sound: Vec<(usize, AudioPacket)>,
}

impl ReplayClip {
    /// Writes the clip as MP4, or MKV when `path` ends in `.mkv`; it plays from 0.
    pub fn write(self, path: &Path) -> Result<(), AvError> {
        let fail =
            |e: std::io::Error| AvError::Encoder(format!("the replay couldn't be written at {} ({e})", path.display()));
        let file = BufWriter::new(File::create(path).map_err(fail)?);
        let mut w = Writer::new(file, is_mkv(path), self.video, vec![self.audio; self.tracks]).map_err(fail)?;
        // In time order, as a recording receives them.
        let mut sound = self.sound.into_iter().peekable();
        for v in &self.packets {
            while let Some((t, a)) = sound.next_if(|(_, a)| a.pts_us <= v.dts_us) {
                w.audio(t, &a).map_err(fail)?;
            }
            w.video(v).map_err(fail)?;
        }
        for (t, a) in sound {
            w.audio(t, &a).map_err(fail)?;
        }
        w.finish().map_err(fail)
    }

    /// Seconds of picture in the clip.
    pub fn seconds(&self) -> f64 {
        match (self.packets.first(), self.packets.last()) {
            (Some(a), Some(b)) => (b.pts_us - a.pts_us) as f64 / 1e6,
            _ => 0.0,
        }
    }
}

#[derive(Debug)]
enum Recording {
    /// Waiting for the encoder's codec header (after the first frame).
    Waiting {
        file: BufWriter<File>,
        mkv: bool,
        /// Sound that came before the header, with its track.
        held: Vec<(usize, AudioPacket)>,
    },
    On(Box<Writer>),
    Failed,
}

/// The file being written: a crash-safe MP4, or MKV when its name ends in `.mkv`.
#[derive(Debug)]
enum Writer {
    Mp4(Mp4Writer<BufWriter<File>>),
    Mkv(MkvWriter<BufWriter<File>>),
}

impl Writer {
    fn new(file: BufWriter<File>, mkv: bool, v: VideoTrack, a: Vec<AudioTrack>) -> std::io::Result<Self> {
        Ok(if mkv {
            Self::Mkv(MkvWriter::with_tracks(file, Some(v), a)?)
        } else {
            Self::Mp4(Mp4Writer::with_tracks(file, Some(v), a)?)
        })
    }

    fn video(&mut self, p: &VideoPacket) -> std::io::Result<()> {
        match self {
            Self::Mp4(w) => w.video(p),
            Self::Mkv(w) => w.video(p),
        }
    }

    fn audio(&mut self, track: usize, p: &AudioPacket) -> std::io::Result<()> {
        match self {
            Self::Mp4(w) => w.audio_on(track, p),
            Self::Mkv(w) => w.audio_on(track, p),
        }
    }

    fn written(&self) -> u64 {
        match self {
            Self::Mp4(w) => w.written(),
            Self::Mkv(w) => w.written(),
        }
    }

    fn chapter(&mut self, name: &str) -> bool {
        match self {
            Self::Mp4(w) => w.chapter(name),
            Self::Mkv(w) => w.chapter(name),
        }
    }

    fn finish(self) -> std::io::Result<()> {
        match self {
            Self::Mp4(w) => w.finish_with_chapters().map(drop),
            Self::Mkv(w) => w.finish_with_chapters().map(drop),
        }
    }
}

/// Whether a recording at `path` is written as MKV (by its name).
fn is_mkv(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mkv"))
}

impl Pipeline {
    /// Starts the encoders; with `record`, also writes an MP4 there (made now, so a path that
    /// can't be written fails at once).
    pub fn new(video: VideoConfig, audio: AudioConfig, record: Option<&Path>) -> Result<Self, AvError> {
        Self::with_tracks(video, audio, record, 0)
    }

    /// As `new`, with `extra` more sound tracks in the recording (fed by `audio_extra`).
    pub fn with_tracks(
        video: VideoConfig,
        audio: AudioConfig,
        record: Option<&Path>,
        extra: usize,
    ) -> Result<Self, AvError> {
        let recording = match record {
            Some(p) => {
                let f = File::create(p).map_err(|e| {
                    AvError::Encoder(format!("the recording can't be written at {} ({e})", p.display()))
                })?;
                Some(Recording::Waiting { file: BufWriter::new(f), mkv: is_mkv(p), held: Vec::new() })
            }
            None => None,
        };
        Ok(Self {
            video: VideoEncoder::new(video)?,
            audio: AudioEncoder::new(audio)?,
            start_ns: None,
            audio_offset_us: None,
            sent_video_header: false,
            sent_audio_header: false,
            recording,
            extra: (0..extra).map(|_| AudioEncoder::new(audio).map(|e| (e, None))).collect::<Result<_, _>>()?,
            paused_at: None,
            paused_ns: 0,
            key_next: false,
            split_to: None,
            replay: None,
        })
    }

    /// Makes this output a replay buffer: it keeps the last `seconds` (and at most `mb`
    /// megabytes, 0: no limit) in memory instead of writing a file; `replay_clip` takes it.
    pub fn keep_replay(&mut self, seconds: u32, mb: u32) {
        self.replay = Some(Replay {
            keep_us: i64::from(seconds.max(1)) * 1_000_000,
            max_bytes: mb as usize * 1024 * 1024,
            video: VecDeque::new(),
            audio: VecDeque::new(),
            bytes: 0,
        });
    }

    /// What the replay buffer holds now, ready to write (None: not a replay buffer, or nothing
    /// kept yet).
    pub fn replay_clip(&self) -> Option<ReplayClip> {
        let r = self.replay.as_ref()?;
        let header = self.video.header()?;
        let cfg = *self.video.config();
        let acfg = *self.audio.config();
        if r.video.is_empty() {
            return None;
        }
        Some(ReplayClip {
            video: VideoTrack { codec: cfg.codec, width: cfg.width, height: cfg.height, config: header },
            audio: AudioTrack {
                sample_rate: acfg.sample_rate,
                channels: acfg.channels,
                config: self.audio.header(),
                bitrate_kbps: acfg.bitrate_kbps,
            },
            tracks: 1 + self.extra.len(),
            packets: r.video.iter().cloned().collect(),
            sound: r.audio.iter().cloned().collect(),
        })
    }

    /// Pauses (`on`) or resumes at `ts_ns`, as OBS pauses a recording: nothing is encoded while
    /// paused, and the file goes on from where it stopped, with no gap. Resuming starts on a
    /// keyframe.
    pub fn pause(&mut self, on: bool, ts_ns: u64) {
        match (on, self.paused_at) {
            (true, None) => self.paused_at = Some(ts_ns),
            (false, Some(at)) => {
                self.paused_ns += ts_ns.saturating_sub(at);
                self.paused_at = None;
                self.key_next = true;
            }
            _ => {}
        }
    }

    pub fn paused(&self) -> bool {
        self.paused_at.is_some()
    }

    /// Goes on in a new file at `path` from the next frame (a keyframe), as OBS's "Split
    /// recording": each file plays on its own from 0. The file is made now, so a path that
    /// can't be written fails at once. Err without a recording.
    pub fn split(&mut self, path: &Path) -> Result<(), AvError> {
        if !matches!(self.recording, Some(Recording::On(_) | Recording::Waiting { .. })) {
            return Err(AvError::Encoder("there's no recording to split".into()));
        }
        if self.split_to.is_some() {
            return Err(AvError::Encoder("a new file is already waiting for its first frame".into()));
        }
        let f = File::create(path)
            .map_err(|e| AvError::Encoder(format!("the next file can't be written at {} ({e})", path.display())))?;
        self.split_to = Some((f, is_mkv(path)));
        self.key_next = true;
        Ok(())
    }

    /// Whether a split waits for its keyframe (the next file isn't written to yet).
    pub fn splitting(&self) -> bool {
        self.split_to.is_some()
    }

    /// Where the file being written now is up to, in bytes (for splitting by size).
    pub fn recorded_bytes(&self) -> u64 {
        match &self.recording {
            Some(Recording::On(w)) => w.written(),
            _ => 0,
        }
    }

    /// A new video bitrate from now on; false when the encoder takes it only at the next start.
    pub fn set_video_kbps(&mut self, kbps: u32) -> Result<bool, AvError> {
        self.video.set_kbps(kbps)
    }

    /// A frame at `ts_ns`. The first one is a keyframe and starts the clock.
    pub fn video(&mut self, frame: Frame<'_>, ts_ns: u64) -> Result<Vec<Tag>, AvError> {
        if self.paused_at.is_some() {
            return Ok(Vec::new());
        }
        let start = *self.start_ns.get_or_insert(ts_ns);
        let first = !self.sent_video_header && self.video.header().is_none();
        let key = first || std::mem::take(&mut self.key_next);
        let pts = (ts_ns.saturating_sub(start).saturating_sub(self.paused_ns) / 1000) as i64;
        let packets = self.video.encode(frame, pts, key)?;
        self.video_out(packets)
    }

    /// Sound (interleaved, the configured channels) whose first sample is at `ts_ns`.
    pub fn audio(&mut self, pcm: &[f32], ts_ns: u64) -> Result<Vec<Tag>, AvError> {
        // Paused: left out, so the sound's own count stays level with the pictures' times.
        if self.paused_at.is_some() {
            return Ok(Vec::new());
        }
        let Some(start) = self.start_ns else { return Ok(Vec::new()) };
        if self.audio_offset_us.is_none() {
            if ts_ns < start {
                // Before the first frame: only the part from the frame on.
                let ch = self.audio.config().channels as u64;
                let rate = u64::from(self.audio.config().sample_rate);
                let skip = ((start - ts_ns) * rate / 1_000_000_000 * ch) as usize;
                if skip >= pcm.len() {
                    return Ok(Vec::new());
                }
                self.audio_offset_us = Some(0);
                return self.audio_encode(&pcm[skip..]);
            }
            self.audio_offset_us = Some(((ts_ns - start) / 1000) as i64);
        }
        self.audio_encode(pcm)
    }

    fn audio_encode(&mut self, pcm: &[f32]) -> Result<Vec<Tag>, AvError> {
        let offset = self.audio_offset_us.unwrap_or(0);
        let mut tags = Vec::new();
        if !self.sent_audio_header {
            self.sent_audio_header = true;
            tags.push(Tag::Audio { ms: 0, body: flv::audio_header(&self.audio.header()) });
        }
        for mut p in self.audio.encode(pcm)? {
            p.pts_us += offset;
            tags.push(Tag::Audio { ms: (p.pts_us / 1000) as u32, body: flv::audio_tag(&p.data) });
            self.record_audio(0, p);
        }
        Ok(tags)
    }

    /// Sound for extra track `i` (0 is the recording's second track), as `audio` takes it.
    /// Recorded only: the stream carries the first track.
    pub fn audio_extra(&mut self, i: usize, pcm: &[f32], ts_ns: u64) -> Result<(), AvError> {
        if self.paused_at.is_some() || i >= self.extra.len() {
            return Ok(());
        }
        let Some(start) = self.start_ns else { return Ok(()) };
        let (enc, offset) = &mut self.extra[i];
        let mut pcm = pcm;
        if offset.is_none() {
            if ts_ns < start {
                let ch = enc.config().channels as u64;
                let rate = u64::from(enc.config().sample_rate);
                let skip = ((start - ts_ns) * rate / 1_000_000_000 * ch) as usize;
                if skip >= pcm.len() {
                    return Ok(());
                }
                *offset = Some(0);
                pcm = &pcm[skip..];
            } else {
                *offset = Some(((ts_ns - start) / 1000) as i64);
            }
        }
        let offset = offset.unwrap_or(0);
        for mut p in enc.encode(pcm)? {
            p.pts_us += offset;
            self.record_audio(i + 1, p);
        }
        Ok(())
    }

    /// Ends: what's still in the encoder is sent, and the recording closed.
    pub fn finish(mut self) -> Result<Vec<Tag>, AvError> {
        let packets = self.video.flush()?;
        let mut tags = self.video_out(packets)?;
        let offset = self.audio_offset_us.unwrap_or(0);
        for mut p in self.audio.flush()? {
            p.pts_us += offset;
            tags.push(Tag::Audio { ms: (p.pts_us / 1000) as u32, body: flv::audio_tag(&p.data) });
            self.record_audio(0, p);
        }
        for i in 0..self.extra.len() {
            let offset = self.extra[i].1.unwrap_or(0);
            for mut p in self.extra[i].0.flush()? {
                p.pts_us += offset;
                self.record_audio(i + 1, p);
            }
        }
        if let Some(Recording::On(w)) = self.recording.take() {
            w.finish().map_err(|e| AvError::Encoder(format!("the recording couldn't be finished ({e})")))?;
        }
        Ok(tags)
    }

    /// A chapter mark in the recording here, named ("Chapter n" when not). Err without a
    /// recording, or before its first frame.
    pub fn chapter(&mut self, name: &str) -> Result<(), AvError> {
        match self.recording.as_mut() {
            Some(Recording::On(w)) => {
                if w.chapter(name) {
                    Ok(())
                } else {
                    Err(AvError::Encoder(format!(
                        "this file has {} chapter marks already, the most it takes (split the recording for more)",
                        crate::mp4::MAX_CHAPTERS
                    )))
                }
            }
            Some(Recording::Waiting { .. }) => Err(AvError::Encoder("the recording hasn't started yet".into())),
            _ => Err(AvError::Encoder("chapter marks go in a recording".into())),
        }
    }

    /// Whether the recording broke (a full disk): the stream goes on without it.
    pub fn recording_failed(&self) -> bool {
        matches!(self.recording, Some(Recording::Failed))
    }

    fn video_out(&mut self, packets: Vec<VideoPacket>) -> Result<Vec<Tag>, AvError> {
        let codec = self.video.config().codec;
        let mut tags = Vec::new();
        for p in packets {
            if !self.sent_video_header {
                let Some(h) = self.video.header() else { continue };
                self.sent_video_header = true;
                tags.push(Tag::Video { ms: 0, body: flv::video_header(codec, &h) });
            }
            let (ms, body) = flv::video_tag(codec, &p, 0);
            tags.push(Tag::Video { ms, body });
            self.record_video(&p);
        }
        Ok(tags)
    }

    fn record_video(&mut self, p: &VideoPacket) {
        if let Some(r) = self.replay.as_mut() {
            r.video(p);
        }
        let header = self.video.header();
        let cfg = *self.video.config();
        let acfg = *self.audio.config();
        let aheader = self.audio.header();
        // A split waiting: the old file is closed and the new one starts on this keyframe.
        if p.key
            && let Some(next) = self.split_to.take()
        {
            if let Some(Recording::On(w)) = self.recording.take()
                && w.finish().is_err()
            {
                self.recording = Some(Recording::Failed);
                return;
            }
            self.recording = Some(Recording::Waiting { file: BufWriter::new(next.0), mkv: next.1, held: Vec::new() });
        }
        let rec = match self.recording.take() {
            Some(Recording::Waiting { file, mkv, held }) => match header {
                Some(config) => {
                    let v = VideoTrack { codec: cfg.codec, width: cfg.width, height: cfg.height, config };
                    let a = AudioTrack {
                        sample_rate: acfg.sample_rate,
                        channels: acfg.channels,
                        config: aheader,
                        bitrate_kbps: acfg.bitrate_kbps,
                    };
                    // Every track the same shape (the extra encoders share the first's settings).
                    let tracks = vec![a; 1 + self.extra.len()];
                    match Writer::new(file, mkv, v, tracks) {
                        Ok(mut w) => {
                            let ok = w.video(p).is_ok() && held.iter().all(|(t, a)| w.audio(*t, a).is_ok());
                            if ok { Recording::On(Box::new(w)) } else { Recording::Failed }
                        }
                        Err(_) => Recording::Failed,
                    }
                }
                None => Recording::Waiting { file, mkv, held },
            },
            Some(Recording::On(mut w)) => {
                if w.video(p).is_ok() {
                    Recording::On(w)
                } else {
                    Recording::Failed
                }
            }
            other => {
                self.recording = other;
                return;
            }
        };
        self.recording = Some(rec);
    }

    fn record_audio(&mut self, track: usize, p: AudioPacket) {
        if let Some(r) = self.replay.as_mut() {
            r.audio(track, &p);
        }
        match &mut self.recording {
            Some(Recording::Waiting { held, .. }) => held.push((track, p)),
            Some(Recording::On(w)) => {
                if w.audio(track, &p).is_err() {
                    self.recording = Some(Recording::Failed);
                }
            }
            _ => {}
        }
    }
}
