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
    /// Its path too: removed when nothing was written to it (stopped before its keyframe).
    split_to: Option<(File, bool, std::path::PathBuf)>,
    /// The replay buffer's memory, when this output is one.
    replay: Option<Replay>,
    /// A stream sends the extra tracks too (Enhanced RTMP multitrack audio).
    stream_extra: bool,
    sent_extra_headers: Vec<bool>,
    /// More video qualities a stream sends as Enhanced RTMP tracks 1… (Twitch Enhanced
    /// Broadcasting): each its encoder, and whether its header went out.
    video_tracks: Vec<(VideoEncoder, bool)>,
    /// A stream that joined a running output (`begin_stream`): nothing goes out before its first
    /// keyframe, whose decode time is its 0 (sound from before it is left out).
    stream_from_us: i64,
    stream_waits_key: bool,
    /// The recording alone paused, its stream going on (they share the encode): each pause
    /// cuts [from, to) out of the file, on the output's clock (µs; `to` None: still paused).
    /// Picture and sound are cut by their time, not by when the encoder hands them back (it
    /// does so a frame or two late; cut that way, the sound fell 0.35 s behind in 4 pauses).
    rec_cuts: Vec<(i64, Option<i64>)>,
    /// Resumed, from this time on (µs): the cut ends at the first keyframe out of the encoder
    /// drawn from then (VideoToolbox makes one when asked; FFmpeg's encoders keep their own
    /// interval, so it may be seconds later: a frame before it needs frames from inside the
    /// cut; and a keyframe drawn while paused, still in the encoder, is paused picture;
    /// projects-60).
    rec_resume: Option<i64>,
    /// Sound that came while the cut's end wasn't known yet (resumed, no keyframe since).
    rec_held: Vec<(usize, AudioPacket)>,
    /// The newest frame given to the encoder (µs).
    last_in_us: i64,
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
            stream_extra: false,
            sent_extra_headers: Vec::new(),
            video_tracks: Vec::new(),
            stream_from_us: 0,
            stream_waits_key: false,
            rec_cuts: Vec::new(),
            rec_resume: None,
            rec_held: Vec::new(),
            last_in_us: 0,
        })
    }

    /// Starts recording into `path` on this running output, at its next frame (a keyframe),
    /// from its own encoders: a recording "Same as stream" beside the stream it shares them
    /// with (one encode, where the Mac's one encoder can't do two at 4K60). The file plays from
    /// 0. Err when it records already or keeps a replay.
    pub fn start_recording(&mut self, path: &Path) -> Result<(), AvError> {
        if self.replay.is_some() {
            return Err(AvError::Encoder("a replay buffer can't record a file too".into()));
        }
        if matches!(self.recording, Some(Recording::On(_) | Recording::Waiting { .. })) || self.split_to.is_some() {
            return Err(AvError::Encoder("this output records already".into()));
        }
        let f = File::create(path)
            .map_err(|e| AvError::Encoder(format!("the recording can't be written at {} ({e})", path.display())))?;
        self.recording = None;
        self.split_to = Some((f, is_mkv(path), path.to_path_buf()));
        self.key_next = true;
        // A recording before it on this output may have stopped paused: this one starts afresh.
        self.rec_cuts.clear();
        self.rec_resume = None;
        self.rec_held.clear();
        Ok(())
    }

    /// Ends this output's recording (its file finished); its encoders and stream go on.
    pub fn end_recording(&mut self) -> Result<(), AvError> {
        self.drop_pending_file();
        match self.recording.take() {
            Some(Recording::On(w)) => {
                w.finish().map_err(|e| AvError::Encoder(format!("the recording couldn't be finished ({e})")))
            }
            _ => Ok(()),
        }
    }

    /// A file made for a recording or split that never got its first frame: an empty file left
    /// in the recordings folder otherwise (projects-60).
    fn drop_pending_file(&mut self) {
        if let Some((f, _, path)) = self.split_to.take() {
            drop(f);
            if std::fs::metadata(&path).is_ok_and(|m| m.len() == 0) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    /// Pauses (`on`) or resumes the recording alone, its stream going on: the encode is shared,
    /// so pausing it all would pause the stream too. The file goes on with no gap.
    pub fn pause_recording(&mut self, on: bool) {
        let open = self.rec_cuts.last().is_some_and(|c| c.1.is_none());
        if on && !open {
            // From the next frame on (every frame drawn before went in already): any time after
            // the newest one, whatever the frame rate (60000/1001 too; projects-60).
            self.rec_cuts.push((self.last_in_us + 1, None));
            self.rec_resume = None;
        } else if on {
            // Paused again before it went on: nothing held is needed.
            self.rec_resume = None;
            self.rec_held.clear();
        } else if open && self.rec_resume.is_none() {
            self.rec_resume = Some(self.last_in_us + 1);
            self.key_next = true;
        }
    }

    /// Whether the recording alone is paused (or waits to go on).
    pub fn recording_paused(&self) -> bool {
        self.rec_cuts.last().is_some_and(|c| c.1.is_none())
    }

    /// Where time `us` lands in the file: None inside a cut, else moved back by the cuts before.
    fn rec_time(&self, us: i64) -> Option<i64> {
        let mut cut = 0;
        for &(from, to) in &self.rec_cuts {
            match to {
                Some(to) if us >= to => cut += to - from,
                _ if us >= from => return None,
                _ => {}
            }
        }
        Some(us - cut)
    }

    /// Whether it writes a file now (or waits for its first frame to).
    pub fn recording(&self) -> bool {
        matches!(self.recording, Some(Recording::On(_) | Recording::Waiting { .. })) || self.split_to.is_some()
    }

    /// A stream joins this running output (a recording "Same as stream" that started first):
    /// its tags start with the codec headers at the next keyframe, which is its time 0. A
    /// keyframe is asked for (FFmpeg's encoders can't be asked: their next one, 2 s at most
    /// with a keyframe interval set).
    pub fn begin_stream(&mut self) -> Result<(), AvError> {
        // Its extra sound or video tracks would go out without their headers and on the old
        // clock (CodeRabbit): an output with them isn't joined (the Studio shares only track 1).
        if !self.extra.is_empty() || !self.video_tracks.is_empty() {
            return Err(AvError::Encoder("a stream joins only an output with one sound track and one picture".into()));
        }
        self.sent_video_header = false;
        self.sent_audio_header = false;
        self.stream_waits_key = true;
        self.key_next = true;
        Ok(())
    }

    /// Whether a stream that joined still waits for its first keyframe.
    pub fn stream_waits_key(&self) -> bool {
        self.stream_waits_key
    }

    /// Which encoder its pictures go through ("h264_nvenc", "VideoToolbox", "libx264"…).
    pub fn encoder(&self) -> String {
        self.video.name()
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
        self.split_to = Some((f, is_mkv(path), path.to_path_buf()));
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

    /// A new picture from the next frame on (size, frame rate, encoder), on a stream that goes
    /// on: the old encoder's last frames come out first, then the new one's header and a
    /// keyframe. The clock and the sound carry on, so the platforms see one stream whose picture
    /// changed (Optimize while live; owner, 15 Nov: "I have to end and stream again"). Streams
    /// only: a file can't change its picture halfway. `next` is opened by the caller (opening a
    /// hardware encoder can take a few hundred ms; nothing else should wait on it).
    pub fn set_video(&mut self, next: VideoEncoder) -> Result<Vec<Tag>, AvError> {
        if self.recording.is_some() || self.replay.is_some() {
            return Err(AvError::Encoder("a recording's picture can't change while it records".into()));
        }
        let packets = self.video.flush()?;
        let tags = self.video_out(packets)?;
        self.video = next;
        self.sent_video_header = false;
        self.key_next = true;
        Ok(tags)
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
        self.last_in_us = pts;
        let packets = self.video.encode(frame, pts, key)?;
        self.video_out(packets)
    }

    /// Adds video qualities a stream sends after its main one (Twitch Enhanced Broadcasting,
    /// docs/plan/TWITCH-EB.md): tracks 1, 2… in the order given. H.264 only. One whose encoder
    /// won't open is left out (the stream goes on with the rest) → how many were added.
    pub fn add_video_tracks(&mut self, configs: &[VideoConfig]) -> usize {
        for c in configs {
            match VideoEncoder::new(*c) {
                Ok(e) => self.video_tracks.push((e, false)),
                Err(e) => {
                    tracing::warn!(width = c.width, height = c.height, error = %e, "a quality left out of the stream")
                }
            }
        }
        self.video_tracks.len()
    }

    /// How many extra video qualities it sends.
    pub fn video_tracks(&self) -> usize {
        self.video_tracks.len()
    }

    /// A frame for extra video track `i` (0 is track 1), at `ts_ns` on the main track's clock:
    /// nothing until the main track has started it.
    pub fn video_track(&mut self, i: usize, frame: Frame<'_>, ts_ns: u64) -> Result<Vec<Tag>, AvError> {
        let Some(start) = self.start_ns else { return Ok(Vec::new()) };
        if self.paused_at.is_some() || i >= self.video_tracks.len() {
            return Ok(Vec::new());
        }
        let pts = (ts_ns.saturating_sub(start).saturating_sub(self.paused_ns) / 1000) as i64;
        let (enc, sent) = &mut self.video_tracks[i];
        let first = !*sent && enc.header().is_none();
        let packets = enc.encode(frame, pts, first)?;
        Ok(Self::track_tags(i, enc, sent, packets))
    }

    fn track_tags(i: usize, enc: &VideoEncoder, sent: &mut bool, packets: Vec<VideoPacket>) -> Vec<Tag> {
        let track = u8::try_from(i + 1).unwrap_or(u8::MAX);
        let mut tags = Vec::new();
        for p in packets {
            if !*sent {
                let Some(h) = enc.header() else { continue };
                *sent = true;
                tags.push(Tag::Video { ms: 0, body: flv::video_track_header(track, &h) });
            }
            let (ms, body) = flv::video_track_tag(track, &p, 0);
            tags.push(Tag::Video { ms, body });
        }
        tags
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
        if !self.sent_audio_header && !self.stream_waits_key {
            self.sent_audio_header = true;
            tags.push(Tag::Audio { ms: 0, body: flv::audio_header(&self.audio.header()) });
        }
        for mut p in self.audio.encode(pcm)? {
            p.pts_us += offset;
            if !self.stream_waits_key && p.pts_us >= self.stream_from_us {
                tags.push(Tag::Audio { ms: self.stream_ms(p.pts_us), body: flv::audio_tag(&p.data) });
            }
            self.record_audio(0, p);
        }
        Ok(tags)
    }

    /// Sound for extra track `i` (0 is the recording's second track), as `audio` takes it.
    /// Recorded only: the stream carries the first track.
    pub fn audio_extra(&mut self, i: usize, pcm: &[f32], ts_ns: u64) -> Result<Vec<Tag>, AvError> {
        if self.paused_at.is_some() || i >= self.extra.len() {
            return Ok(Vec::new());
        }
        let Some(start) = self.start_ns else { return Ok(Vec::new()) };
        let (enc, offset) = &mut self.extra[i];
        let mut pcm = pcm;
        if offset.is_none() {
            if ts_ns < start {
                let ch = enc.config().channels as u64;
                let rate = u64::from(enc.config().sample_rate);
                let skip = ((start - ts_ns) * rate / 1_000_000_000 * ch) as usize;
                if skip >= pcm.len() {
                    return Ok(Vec::new());
                }
                *offset = Some(0);
                pcm = &pcm[skip..];
            } else {
                *offset = Some(((ts_ns - start) / 1000) as i64);
            }
        }
        let offset = offset.unwrap_or(0);
        let packets = enc.encode(pcm)?;
        let mut tags = Vec::new();
        for mut p in packets {
            p.pts_us += offset;
            tags.extend(self.extra_tag(i, &p));
            self.record_audio(i + 1, p);
        }
        Ok(tags)
    }

    /// Makes a stream send its extra sound tracks too (track ids 1, 2… after the main one),
    /// as OBS sends Twitch's VOD track.
    pub fn stream_extra_tracks(&mut self) {
        self.stream_extra = true;
        self.sent_extra_headers = vec![false; self.extra.len()];
    }

    /// An extra track's packet as stream tags (its header first), when streamed.
    fn extra_tag(&mut self, i: usize, p: &AudioPacket) -> Vec<Tag> {
        if !self.stream_extra {
            return Vec::new();
        }
        let track = u8::try_from(i + 1).unwrap_or(u8::MAX);
        let mut tags = Vec::new();
        if let Some(sent) = self.sent_extra_headers.get_mut(i)
            && !*sent
        {
            *sent = true;
            let header = self.extra[i].0.header();
            tags.push(Tag::Audio { ms: 0, body: flv::audio_track_tag(track, true, &header) });
        }
        let ms = (p.pts_us.max(0) / 1000) as u32;
        tags.push(Tag::Audio { ms, body: flv::audio_track_tag(track, false, &p.data) });
        tags
    }

    /// Ends: what's still in the encoder is sent, and the recording closed.
    pub fn finish(self) -> Result<Vec<Tag>, AvError> {
        // Every FFmpeg in it (video, sound, extra tracks) shares one stop time.
        let by = std::time::Instant::now() + crate::ffenc::FINISH_WITHIN;
        crate::ffenc::finishing_by(by, move || self.finish_now())
    }

    fn finish_now(mut self) -> Result<Vec<Tag>, AvError> {
        let tags = self.flush_all();
        // Whatever the encoders said: no empty file left waiting for a keyframe, and the file
        // written finished, so it plays (CodeRabbit: a failed flush left both).
        self.drop_pending_file();
        let finished = match self.recording.take() {
            Some(Recording::On(w)) => {
                w.finish().map_err(|e| AvError::Encoder(format!("the recording couldn't be finished ({e})")))
            }
            _ => Ok(()),
        };
        let tags = tags?;
        finished?;
        Ok(tags)
    }

    /// The encoders' last packets, sent and written.
    fn flush_all(&mut self) -> Result<Vec<Tag>, AvError> {
        let packets = self.video.flush()?;
        let mut tags = self.video_out(packets)?;
        let offset = self.audio_offset_us.unwrap_or(0);
        for mut p in self.audio.flush()? {
            p.pts_us += offset;
            if !self.stream_waits_key && p.pts_us >= self.stream_from_us {
                tags.push(Tag::Audio { ms: self.stream_ms(p.pts_us), body: flv::audio_tag(&p.data) });
            }
            self.record_audio(0, p);
        }
        for i in 0..self.extra.len() {
            let offset = self.extra[i].1.unwrap_or(0);
            for mut p in self.extra[i].0.flush()? {
                p.pts_us += offset;
                tags.extend(self.extra_tag(i, &p));
                self.record_audio(i + 1, p);
            }
        }
        for i in 0..self.video_tracks.len() {
            let (enc, sent) = &mut self.video_tracks[i];
            let packets = enc.flush()?;
            tags.extend(Self::track_tags(i, enc, sent, packets));
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

    /// A time on the output's clock (µs) as the stream's tag time (ms).
    fn stream_ms(&self, us: i64) -> u32 {
        ((us - self.stream_from_us).max(0) / 1000) as u32
    }

    fn video_out(&mut self, packets: Vec<VideoPacket>) -> Result<Vec<Tag>, AvError> {
        let codec = self.video.config().codec;
        let mut tags = Vec::new();
        for p in packets {
            // A stream that joined goes out from its first keyframe, timed from its decode time
            // (the encoder's frames still under way come out as P-frames first: left out).
            if self.stream_waits_key {
                if !p.key {
                    self.record_video(&p);
                    continue;
                }
                self.stream_waits_key = false;
                self.stream_from_us = p.dts_us;
            }
            let (ms, body) = flv::video_tag(codec, &p, self.stream_from_us);
            if !self.sent_video_header {
                let Some(h) = self.video.header() else { continue };
                self.sent_video_header = true;
                // At its first frame's time: 0 when the stream starts, and where the stream is
                // when a new picture takes over (`set_video`) so time never goes back.
                tags.push(Tag::Video { ms, body: flv::video_header(codec, &h) });
            }
            tags.push(Tag::Video { ms, body });
            self.record_video(&p);
        }
        Ok(tags)
    }

    fn record_video(&mut self, p: &VideoPacket) {
        if let Some(r) = self.replay.as_mut() {
            r.video(p);
        }
        // Resumed after a recording-only pause: the cut ends at the first keyframe out; frames
        // before it aren't written (they lean on frames inside the cut).
        let mut resumed = false;
        if let Some(c) = self.rec_cuts.last_mut()
            && c.1.is_none()
        {
            match cut_step(c.0, self.rec_resume, p.key, p.pts_us) {
                CutStep::Write => {}
                CutStep::Leave => return,
                CutStep::End => {
                    c.1 = Some(p.pts_us);
                    self.rec_resume = None;
                    resumed = true;
                }
            }
        }
        self.write_video(p);
        // The sound held meanwhile, cut by the same line, after the keyframe (in the file it
        // went to, a split landing on it included).
        if resumed {
            for (track, a) in std::mem::take(&mut self.rec_held) {
                self.record_audio(track, a);
            }
        }
    }

    fn write_video(&mut self, p: &VideoPacket) {
        // The recording alone paused: frames in a cut aren't written, later ones move back.
        let shifted;
        let p = if self.rec_cuts.is_empty() {
            p
        } else {
            let Some(at) = self.rec_time(p.pts_us) else { return };
            let by = p.pts_us - at;
            shifted = VideoPacket { pts_us: at, dts_us: p.dts_us - by, ..p.clone() };
            &shifted
        };
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

    fn record_audio(&mut self, track: usize, mut p: AudioPacket) {
        if let Some(r) = self.replay.as_mut() {
            r.audio(track, &p);
        }
        if !self.rec_cuts.is_empty() {
            // Resumed, the cut's end not known yet: held until the next frame says it.
            if self.rec_resume.is_some() && self.rec_cuts.last().is_some_and(|c| c.1.is_none() && p.pts_us >= c.0) {
                self.rec_held.push((track, p));
                return;
            }
            let Some(at) = self.rec_time(p.pts_us) else { return };
            p.pts_us = at;
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

/// What a frame out of the encoder does while a recording-only pause's cut is open (from
/// `from`; resumed from `resume`, if it was).
#[derive(Debug, PartialEq, Eq)]
enum CutStep {
    /// Drawn before the pause: written.
    Write,
    /// Inside the cut: left out.
    Leave,
    /// The keyframe the recording goes on at: it ends the cut.
    End,
}

fn cut_step(from: i64, resume: Option<i64>, key: bool, pts: i64) -> CutStep {
    match resume {
        _ if pts < from => CutStep::Write,
        Some(at) if key && pts >= at => CutStep::End,
        _ => CutStep::Leave,
    }
}

#[cfg(test)]
mod cut_tests {
    use super::{CutStep, cut_step};

    /// A recording-only pause from 3.0 s, resumed at 3.9 s: frames from before 3.0 s are
    /// written; the cut ends only at a keyframe drawn from 3.9 s on, never at one drawn while
    /// paused and still in the encoder (FFmpeg's lookahead keeps up to a second; that would put
    /// paused picture in the file), nor at a frame that isn't one (FFmpeg's encoders don't make
    /// a keyframe when asked; it would lean on frames left out; projects-60).
    #[test]
    fn the_cut_ends_only_at_a_keyframe_drawn_after_the_resume() {
        let (from, at) = (3_000_000, 3_900_000);
        assert_eq!(cut_step(from, None, false, 2_966_667), CutStep::Write);
        assert_eq!(cut_step(from, Some(at), false, 2_966_667), CutStep::Write);
        assert_eq!(cut_step(from, None, true, 3_500_000), CutStep::Leave, "still paused");
        assert_eq!(cut_step(from, Some(at), true, 3_800_000), CutStep::Leave, "a keyframe drawn while paused");
        assert_eq!(cut_step(from, Some(at), false, 3_933_333), CutStep::Leave, "not a keyframe");
        assert_eq!(cut_step(from, Some(at), true, 3_900_000), CutStep::End);
        assert_eq!(cut_step(from, Some(at), true, 4_000_000), CutStep::End);
    }
}
