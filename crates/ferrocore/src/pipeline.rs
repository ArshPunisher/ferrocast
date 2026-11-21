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
use crate::mp4::{AudioTrack, Flavor, Mp4Writer, VideoTrack};
use crate::video::{Frame, VideoConfig, VideoEncoder, VideoPacket};

/// Maximum audio packets held while waiting for the first video codec header or post-resume
/// keyframe (~10.9 s at 48 kHz AAC) so a stalled video encoder never grows RAM without bound.
const MAX_HELD_AUDIO: usize = 512;

fn push_held_audio(q: &mut VecDeque<(usize, AudioPacket)>, item: (usize, AudioPacket)) {
    while q.len() >= MAX_HELD_AUDIO {
        q.pop_front();
    }
    q.push_back(item);
}

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
    /// Tags made when the sound was settled outside `audio` (a pause, a file change): sent with
    /// the next call's.
    carry: Vec<Tag>,
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
    /// Tests: every video frame fails, as a faulting encoder's would.
    #[cfg(test)]
    fail_video: bool,
    /// Tests: the next video frame fails once.
    #[cfg(test)]
    fail_video_once: bool,
    #[cfg(test)]
    video_calls: u64,
    /// Time spent paused so far, taken off every later frame's time (no gap in the file).
    paused_ns: u64,
    /// The next frame is made a keyframe (after a pause, or to start a split file on).
    key_next: bool,
    /// A new file to go on in at the next keyframe ("Split recording").
    /// Its path too: removed when nothing was written to it (stopped before its keyframe).
    split_to: Option<(File, Kind, std::path::PathBuf)>,
    /// The video encoder was rebuilt with other parameter sets: the file goes on in a new part
    /// from the rebuilt session's first keyframe (one avcC per file). Its frames wait in
    /// `split_held` until the caller names the file (`split`).
    split_wanted: bool,
    split_held: VecDeque<VideoPacket>,
    /// How many times a recording went on in a new file for that reason.
    auto_splits: u32,
    /// The replay buffer's memory, when this output is one.
    replay: Option<Replay>,
    /// A stream sends the extra tracks too (Enhanced RTMP multitrack audio).
    stream_extra: bool,
    /// The recording's audio tracks' names, by track ("Mic", "Game"); a missing one has none.
    track_names: Vec<String>,
    sent_extra_headers: Vec<bool>,
    /// More video qualities a stream sends as Enhanced RTMP tracks 1… (Twitch Enhanced
    /// Broadcasting): each its encoder, and whether its header went out.
    video_tracks: Vec<(VideoEncoder, bool)>,
    /// A stream that joined a running output (`begin_stream`): nothing goes out before its first
    /// keyframe, whose decode time is its 0 (sound from before it is left out).
    stream_from_us: i64,
    stream_waits_key: bool,
    /// The recording alone paused, its stream going on (they share the encode): each pause
    /// cuts [from, to) out of the file on the raw clock and shifts later media by `cut`, leaving
    /// one frame interval after the last pre-cut frame so resuming never collides with it in MP4.
    /// Picture and sound are cut by their time, not by when the encoder hands them back (it
    /// does so a frame or two late; cut that way, the sound fell 0.35 s behind in 4 pauses).
    rec_cuts: Vec<RecCut>,
    /// Resumed, from this time on (µs): the cut ends at the first keyframe out of the encoder
    /// drawn from then (VideoToolbox makes one when asked; FFmpeg's encoders keep their own
    /// interval, so it may be seconds later: a frame before it needs frames from inside the
    /// cut; and a keyframe drawn while paused, still in the encoder, is paused picture;
    /// projects-60).
    rec_resume: Option<i64>,
    /// True once the active open recording cut has started dropping packets (`pts >= from`), so
    /// reordered B-frames with `pts < from` whose reference was already dropped are not orphaned.
    rec_cut_entered: bool,
    /// Highest raw presentation timestamp written to the recording so far, to place resume cuts cleanly.
    rec_last_raw_pts_us: Option<i64>,
    /// Last decode timestamp written to the recording, to keep recording DTS monotonic across cuts.
    rec_last_dts_us: Option<i64>,
    /// Last decode timestamp produced by `self.video`, carried across `set_video` and `heal`.
    last_video_dts_us: Option<i64>,
    /// Logged once if the last-resort DTS clamp in `video_out` ever fires.
    dts_clamp_logged: bool,
    /// Sound that came while the cut's end wasn't known yet (resumed, no keyframe since).
    rec_held: VecDeque<(usize, AudioPacket)>,
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
        // Enforce time window and byte cap even if video stalls and stops delivering keyframes
        // (matching OBS `replay_buffer_purge` trimming audio alongside video).
        let newest = p.pts_us;
        let audio_keep = self.keep_us.saturating_add(2_000_000);
        while self.audio.len() > 1 {
            let too_old = self.audio.front().is_some_and(|(_, a)| newest.saturating_sub(a.pts_us) > audio_keep);
            let too_big = self.max_bytes > 0 && self.bytes > self.max_bytes;
            if !(too_old || too_big) {
                break;
            }
            if let Some((_, a)) = self.audio.pop_front() {
                self.bytes = self.bytes.saturating_sub(a.data.len());
            }
        }
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
        let mut w = Writer::new(file, kind_of(path), self.video, vec![self.audio; self.tracks]).map_err(fail)?;
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
        w.finish().map_err(|u| fail(u.cut()))
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
        kind: Kind,
        /// Sound that came before the header, with its track.
        held: VecDeque<(usize, AudioPacket)>,
    },
    On(Box<Writer>),
    /// The file couldn't be written (a full disk, a drive taken away): what is in it was kept,
    /// nothing more is written, and nothing is tried again.
    Failed(RecordProblem),
}

/// Why a recording stopped, as a short phrase for the line ("the disk is full") and the rest
/// (what to do, and the system's own words) for behind the (i).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordProblem {
    pub reason: String,
    pub detail: String,
    /// How many bytes of the file were kept (its size, for the Stats that go on showing it).
    pub kept: u64,
}

impl RecordProblem {
    /// What an I/O error from writing the file means to the person recording.
    pub fn from_io(e: &std::io::Error) -> Self {
        use std::io::ErrorKind as K;
        let os = e.raw_os_error();
        // ENOSPC (28), Windows' disk full (112) and handle disk full (39).
        let full = e.kind() == K::StorageFull || matches!(os, Some(28 | 112 | 39));
        // A disk quota: EDQUOT (Linux 122, macOS 69), Windows' not enough quota (1816).
        let quota = e.kind() == K::QuotaExceeded || matches!(os, Some(122 | 69 | 1816));
        // A drive pulled out or gone: ENODEV, ENXIO, Windows' not ready, no such device,
        // network name deleted, I/O device error, device not connected.
        #[cfg(unix)]
        let gone_code = matches!(os, Some(19 | 6));
        #[cfg(windows)]
        let gone_code = matches!(os, Some(21 | 55 | 64 | 1117 | 1167));
        #[cfg(not(any(unix, windows)))]
        let gone_code = false;
        let gone = matches!(e.kind(), K::NotFound | K::BrokenPipe | K::NotConnected) || gone_code;
        let denied = matches!(e.kind(), K::PermissionDenied | K::ReadOnlyFilesystem) || os == Some(30);
        let (reason, advice) = if full {
            ("the disk is full", "Free some space or choose another drive, then record again.")
        } else if quota {
            ("the disk quota is used up", "Free some space or choose another drive, then record again.")
        } else if gone {
            ("the drive isn't there anymore", "Connect it again or choose another folder, then record again.")
        } else if denied {
            ("the folder can't be written", "Choose a folder you can write to, then record again.")
        } else {
            ("the file couldn't be written", "Check the drive, or choose another folder, then record again.")
        };
        let said: String = e.to_string().chars().take(200).collect();
        RecordProblem {
            reason: reason.into(),
            detail: format!("{advice} What was recorded until now is saved and plays. ({said})"),
            kept: 0,
        }
    }
}

/// Stops writing a file that can't be written: what was whole in it is kept (the half-written
/// piece at its end is cut off, so it plays), and the reason is logged once.
fn give_up(w: Box<Writer>, e: &std::io::Error) -> RecordProblem {
    let (file, whole) = w.abandon();
    salvage(file, whole);
    let problem = RecordProblem { kept: whole, ..RecordProblem::from_io(e) };
    tracing::error!(error = %e, reason = %problem.reason, kept = whole, "the recording stopped; the file so far was kept");
    problem
}

/// Cuts `file` back to `whole` bytes (a fragment cut short by a full disk would end it), and
/// asks the system to write out what it has. Shrinking a file needs no room. Every step may
/// fail without harm: nothing here panics or retries.
fn salvage(file: BufWriter<File>, whole: u64) {
    // What the buffer still holds belongs to a piece that never went out.
    let (f, _unwritten) = file.into_parts();
    if f.metadata().is_ok_and(|m| m.len() > whole) {
        let _ = f.set_len(whole);
    }
    let _ = f.sync_all();
}

/// The file being written: a crash-safe MP4, or MKV when its name ends in `.mkv`.
#[derive(Debug)]
enum Writer {
    Mp4(Mp4Writer<BufWriter<File>>),
    Mkv(MkvWriter<BufWriter<File>>),
}

impl Writer {
    fn new(file: BufWriter<File>, kind: Kind, v: VideoTrack, a: Vec<AudioTrack>) -> std::io::Result<Self> {
        Ok(match kind {
            Kind::Mkv => Self::Mkv(MkvWriter::with_tracks(file, Some(v), a)?),
            Kind::Mov => Self::Mp4(Mp4Writer::with_flavor(file, Some(v), a, Flavor::Mov)?),
            Kind::Mp4 => Self::Mp4(Mp4Writer::with_tracks(file, Some(v), a)?),
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

    /// Gives the file up without writing what waits: it, and where its last whole piece ends.
    fn abandon(self) -> (BufWriter<File>, u64) {
        match self {
            Self::Mp4(w) => w.abandon(),
            Self::Mkv(w) => w.abandon(),
        }
    }

    /// Makes the file a regular MP4, MOV or MKV (see `Mp4Writer::finalize`). On a failure the file
    /// is as it was, and the failure keeps the file for cutting back.
    fn finish(self) -> Result<(), Unfinished> {
        let unfinished =
            |f: crate::mp4::FinishFailed<BufWriter<File>>| Unfinished { error: f.error, file: f.out, whole: f.whole };
        match self {
            Self::Mp4(w) => w.finalize().map(drop).map_err(unfinished),
            Self::Mkv(w) => w.finalize().map(drop).map_err(unfinished),
        }
    }
}

/// A file that couldn't be finished: why, the file, and where its last whole piece ends.
struct Unfinished {
    error: std::io::Error,
    file: BufWriter<File>,
    whole: u64,
}

impl Unfinished {
    /// Cuts the file back to its last whole piece (so it plays as it was, open-ended) and gives
    /// the error.
    fn cut(self) -> std::io::Error {
        salvage(self.file, self.whole);
        self.error
    }
}

/// What a recording's file is, by its name: MKV, MOV (QuickTime) or MP4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Mp4,
    Mov,
    Mkv,
}

fn kind_of(path: &Path) -> Kind {
    match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("mkv") => Kind::Mkv,
        Some("mov") => Kind::Mov,
        _ => Kind::Mp4,
    }
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
                Some(Recording::Waiting { file: BufWriter::new(f), kind: kind_of(p), held: VecDeque::new() })
            }
            None => None,
        };
        let video = VideoEncoder::new(video)?;
        let stream_from_us = -video.dts_shift();
        Ok(Self {
            video,
            audio: AudioEncoder::new(audio)?,
            carry: Vec::new(),
            start_ns: None,
            audio_offset_us: None,
            sent_video_header: false,
            sent_audio_header: false,
            recording,
            extra: (0..extra).map(|_| AudioEncoder::new(audio).map(|e| (e, None))).collect::<Result<_, _>>()?,
            paused_at: None,
            #[cfg(test)]
            fail_video: false,
            #[cfg(test)]
            fail_video_once: false,
            #[cfg(test)]
            video_calls: 0,
            paused_ns: 0,
            key_next: false,
            split_to: None,
            split_wanted: false,
            split_held: VecDeque::new(),
            auto_splits: 0,
            replay: None,
            stream_extra: false,
            track_names: Vec::new(),
            sent_extra_headers: Vec::new(),
            video_tracks: Vec::new(),
            stream_from_us,
            stream_waits_key: false,
            rec_cuts: Vec::new(),
            rec_resume: None,
            rec_cut_entered: false,
            rec_last_raw_pts_us: None,
            rec_last_dts_us: None,
            last_video_dts_us: None,
            dts_clamp_logged: false,
            rec_held: VecDeque::new(),
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
        self.split_to = Some((f, kind_of(path), path.to_path_buf()));
        self.key_next = true;
        // A recording before it on this output may have stopped paused: this one starts afresh.
        self.rec_cuts.clear();
        self.rec_resume = None;
        self.rec_cut_entered = false;
        self.rec_last_raw_pts_us = None;
        self.rec_last_dts_us = None;
        self.rec_held.clear();
        Ok(())
    }

    /// Ends this output's recording (its file finished); its encoders and stream go on.
    pub fn end_recording(&mut self) -> Result<(), AvError> {
        // The sound still being encoded goes into the file first.
        self.settle_logged();
        self.drop_pending_file();
        match self.recording.take() {
            Some(Recording::On(w)) => {
                w.finish().map_err(|u| AvError::Encoder(format!("the recording couldn't be finished ({})", u.cut())))
            }
            _ => Ok(()),
        }
    }

    /// A file made for a recording or split that never got its first frame: an empty file left
    /// in the recordings folder otherwise (projects-60).
    fn drop_pending_file(&mut self) {
        if let Some((f, _, path)) = self.split_to.take() {
            drop(f);
            Self::forget_empty(&path);
        }
    }

    /// Removes `path` when nothing was written to it.
    fn forget_empty(path: &Path) {
        if std::fs::metadata(path).is_ok_and(|m| m.len() == 0) {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Pauses (`on`) or resumes the recording alone, its stream going on: the encode is shared,
    /// so pausing it all would pause the stream too. The file goes on with no gap.
    pub fn pause_recording(&mut self, on: bool) {
        self.settle_logged();
        let open = self.rec_cuts.last().is_some_and(|c| c.to.is_none());
        if on && !open {
            // From the next frame on (every frame drawn before went in already): any time after
            // the newest one, whatever the frame rate (60000/1001 too; projects-60).
            self.rec_cuts.push(RecCut { from: self.last_in_us + 1, to: None, cut: 0 });
            self.rec_resume = None;
            self.rec_cut_entered = false;
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
        self.rec_cuts.last().is_some_and(|c| c.to.is_none())
    }

    /// Where time `us` lands in the file: None inside a cut, else moved back by the cuts before.
    fn rec_time(&self, us: i64) -> Option<i64> {
        let mut cut = 0;
        for c in &self.rec_cuts {
            match c.to {
                Some(to) if us >= to => cut += c.cut,
                _ if us >= c.from => return None,
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
                name: String::new(),
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
        self.settle_logged();
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
        if let Some(Recording::Failed(p)) = &self.recording {
            return Err(AvError::Encoder(format!("the recording stopped: {}", p.reason)));
        }
        if !matches!(self.recording, Some(Recording::On(_) | Recording::Waiting { .. })) {
            return Err(AvError::Encoder("there's no recording to split".into()));
        }
        if self.split_to.is_some() {
            return Err(AvError::Encoder("a new file is already waiting for its first frame".into()));
        }
        self.settle_logged();
        let f = File::create(path)
            .map_err(|e| AvError::Encoder(format!("the next file can't be written at {} ({e})", path.display())))?;
        self.split_to = Some((f, kind_of(path), path.to_path_buf()));
        self.key_next = true;
        Ok(())
    }

    /// A rebuilt video encoder needs the recording in a new file, and is waiting to be told where
    /// (`split`). Its frames are held meanwhile.
    pub fn split_wanted(&self) -> bool {
        self.split_wanted
    }

    /// Recordings that went on in a new file because the encoder was rebuilt with other
    /// parameter sets.
    pub fn auto_splits(&self) -> u32 {
        self.auto_splits
    }

    /// Whether a split waits for its keyframe (the next file isn't written to yet).
    pub fn splitting(&self) -> bool {
        self.split_to.is_some()
    }

    /// Where the file being written now is up to, in bytes (for splitting by size).
    pub fn recorded_bytes(&self) -> u64 {
        match &self.recording {
            Some(Recording::On(w)) => w.written(),
            // What was kept of a recording that stopped: its size goes on showing.
            Some(Recording::Failed(p)) => p.kept,
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
    pub fn set_video(&mut self, mut next: VideoEncoder) -> Result<Vec<Tag>, AvError> {
        if self.recording.is_some() || self.replay.is_some() {
            return Err(AvError::Encoder("a recording's picture can't change while it records".into()));
        }
        let packets = self.video.flush()?;
        let tags = self.video_out(packets)?;
        if let Some(last) = self.last_video_dts_us {
            next.set_min_dts(last);
        }
        self.video = next;
        self.sent_video_header = false;
        self.key_next = true;
        Ok(tags)
    }

    /// Makes the encoders again after one failed, and goes on: a stream sends its new header and
    /// a keyframe from the next frame (the clock and the sound carry on); a recording asks for a
    /// new part (`split_wanted`, the caller names the file), since a file can't change its codec
    /// config halfway. The old encoders are dropped as they are: they failed, nothing is
    /// flushed out of them.
    pub fn heal(&mut self) -> Result<(), AvError> {
        let mut video = VideoEncoder::new(*self.video.config())?;
        if let Some(last) = self.last_video_dts_us {
            video.set_min_dts(last);
        }
        let mut audio = AudioEncoder::new(*self.audio.config())?;
        if self.audio.is_off_thread() {
            audio = audio.off_thread()?;
        }
        self.video = video;
        self.audio = audio;
        // The new encoder counts its sound from 0: its start on the output's clock is found again
        // at the next sound (before, the old one's stayed and sound went out at time 0 again).
        self.audio_offset_us = None;
        self.sent_video_header = false;
        self.sent_audio_header = false;
        self.key_next = true;
        if matches!(self.recording, Some(Recording::On(_) | Recording::Waiting { .. })) {
            self.split_wanted = true;
        }
        Ok(())
    }

    /// A frame at `ts_ns`. The first one is a keyframe and starts the clock.
    pub fn video(&mut self, frame: Frame<'_>, ts_ns: u64) -> Result<Vec<Tag>, AvError> {
        let mut tags = std::mem::take(&mut self.carry);
        // Where the sound goes depends on what this frame's packets do (a keyframe starts a file,
        // a stream, ends a cut): the sound given before it is settled first, so it lands as when
        // the sound is encoded in this thread.
        if self.audio_changes_course() {
            self.settle_audio()?;
            tags.append(&mut self.carry);
        }
        tags.extend(self.video_now(frame, ts_ns)?);
        Ok(tags)
    }

    /// Whether a frame may move where sound goes.
    fn audio_changes_course(&self) -> bool {
        self.audio.is_off_thread()
            && (self.split_to.is_some()
                || self.stream_waits_key
                || self.rec_resume.is_some()
                || matches!(self.recording, Some(Recording::Waiting { .. }))
                || self.split_wanted)
    }

    /// Has the sound encode on threads of its own (the render thread then only queues it). Call
    /// before the first sample.
    pub fn audio_off_thread(&mut self) -> Result<(), AvError> {
        // The encoders already made move over as they are (a failure leaves each where it was).
        self.audio.move_off_thread()?;
        for (e, _) in &mut self.extra {
            e.move_off_thread()?;
        }
        Ok(())
    }

    /// `settle_audio` where the caller has no way to report: a failure resurfaces on the next call.
    fn settle_logged(&mut self) {
        if let Err(e) = self.settle_audio() {
            tracing::warn!(error = %e, "the sound encoder failed while settling");
        }
    }

    /// Waits for the sound given so far and files what it made (into `carry`, to go out with the
    /// next call): at a pause or file change, so packets land on the right side of it.
    fn settle_audio(&mut self) -> Result<(), AvError> {
        if !self.audio.is_off_thread() {
            return Ok(());
        }
        let offset = self.audio_offset_us.unwrap_or(0);
        let main = self.audio.settle()?;
        self.main_audio_out(main, offset);
        for i in 0..self.extra.len() {
            let offset = self.extra[i].1.unwrap_or(0);
            let made = self.extra[i].0.settle()?;
            self.extra_audio_out(i, made, offset);
        }
        Ok(())
    }

    fn main_audio_out(&mut self, packets: Vec<AudioPacket>, offset: i64) {
        if !packets.is_empty() && !self.sent_audio_header && !self.stream_waits_key {
            // The codec header goes before the first sound, whichever call brings it.
            self.sent_audio_header = true;
            self.carry.push(Tag::Audio { ms: 0, body: flv::audio_header(&self.audio.header()) });
        }
        for mut p in packets {
            p.pts_us += offset;
            if !self.stream_waits_key && p.pts_us >= self.stream_from_us {
                let tag = Tag::Audio { ms: self.stream_ms(p.pts_us), body: flv::audio_tag(&p.data) };
                self.carry.push(tag);
            }
            self.record_audio(0, p);
        }
    }

    fn extra_audio_out(&mut self, i: usize, packets: Vec<AudioPacket>, offset: i64) {
        for mut p in packets {
            p.pts_us += offset;
            let tags = self.extra_tag(i, &p);
            self.carry.extend(tags);
            self.record_audio(i + 1, p);
        }
    }

    fn video_now(&mut self, frame: Frame<'_>, ts_ns: u64) -> Result<Vec<Tag>, AvError> {
        if self.paused_at.is_some() {
            return Ok(Vec::new());
        }
        #[cfg(test)]
        {
            self.video_calls += 1;
        }
        #[cfg(test)]
        if std::mem::take(&mut self.fail_video_once) {
            return Err(AvError::Source("an injected encoder fault (once)".into()));
        }
        #[cfg(test)]
        if self.fail_video {
            return Err(AvError::Source("an injected encoder fault".into()));
        }
        let start = *self.start_ns.get_or_insert(ts_ns);
        let first = !self.sent_video_header && self.video.header().is_none();
        let key = first || std::mem::take(&mut self.key_next);
        let pts = (ts_ns.saturating_sub(start).saturating_sub(self.paused_ns) / 1000) as i64;
        self.last_in_us = pts;
        let packets = self.video.encode(frame, pts, key)?;
        // A hardware session the system took back and we rebuilt: if its parameter sets differ,
        // the stream sends a new sequence header before its keyframe, as when a new picture
        // takes over (`set_video`). A file can't change its codec config: a recording goes on in a new
        // file from the new session's keyframe (`split_wanted`).
        if self.video.header_changed() {
            if matches!(self.recording, Some(Recording::On(_) | Recording::Waiting { .. })) {
                tracing::warn!(
                    "the video encoder was rebuilt with other parameter sets; the recording goes on in a new file"
                );
                self.split_wanted = true;
            } else if self.recording.is_some() || self.replay.is_some() {
                tracing::warn!("the video encoder was rebuilt with other parameter sets; the file keeps the first");
            }
            self.sent_video_header = false;
        }
        self.video_out(packets)
    }

    /// Adds video qualities a stream sends after its main one (Twitch Enhanced Broadcasting,
    /// docs/plan/TWITCH-EB.md): tracks 1, 2… in the order given. H.264 only. One whose encoder
    /// won't open is left out (the stream goes on with the rest) → how many were added.
    pub fn add_video_tracks(&mut self, configs: &[VideoConfig]) -> usize {
        for c in configs {
            match VideoEncoder::new(*c) {
                Ok(e) => {
                    if !self.sent_video_header && !self.stream_waits_key {
                        self.stream_from_us = self.stream_from_us.min(-e.dts_shift());
                    }
                    self.video_tracks.push((e, false));
                }
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
        let start_us = self.stream_from_us;
        let (enc, sent) = &mut self.video_tracks[i];
        let first = !*sent && enc.header().is_none();
        let packets = enc.encode(frame, pts, first)?;
        // A rebuilt hardware session with other parameter sets: this quality's new sequence
        // header goes out before its keyframe, as the main track's does.
        if enc.header_changed() {
            *sent = false;
        }
        Ok(Self::track_tags(i, enc, sent, packets, start_us))
    }

    fn track_tags(i: usize, enc: &VideoEncoder, sent: &mut bool, packets: Vec<VideoPacket>, start_us: i64) -> Vec<Tag> {
        let track = u8::try_from(i + 1).unwrap_or(u8::MAX);
        let mut tags = Vec::new();
        for p in packets {
            if !*sent {
                let Some(h) = enc.header() else { continue };
                *sent = true;
                let ms = ((p.dts_us - start_us).max(0) / 1000) as u32;
                tags.push(Tag::Video { ms, body: flv::video_track_header(track, &h) });
            }
            let (ms, body) = flv::video_track_tag(track, &p, start_us);
            tags.push(Tag::Video { ms, body });
        }
        tags
    }

    /// Sound (interleaved, the configured channels) whose first sample is at `ts_ns`.
    pub fn audio(&mut self, pcm: &[f32], ts_ns: u64) -> Result<Vec<Tag>, AvError> {
        let mut tags = std::mem::take(&mut self.carry);
        tags.extend(self.audio_now(pcm, ts_ns)?);
        Ok(tags)
    }

    fn audio_now(&mut self, pcm: &[f32], ts_ns: u64) -> Result<Vec<Tag>, AvError> {
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
        let mut tags = std::mem::take(&mut self.carry);
        tags.extend(self.audio_extra_now(i, pcm, ts_ns)?);
        Ok(tags)
    }

    fn audio_extra_now(&mut self, i: usize, pcm: &[f32], ts_ns: u64) -> Result<Vec<Tag>, AvError> {
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

    /// The names the recording's audio tracks get in the file (first: track one).
    pub fn set_track_names(&mut self, names: Vec<String>) {
        self.track_names = names;
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
        let ms = self.stream_ms(p.pts_us);
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
                w.finish().map_err(|u| AvError::Encoder(format!("the recording couldn't be finished ({})", u.cut())))
            }
            _ => Ok(()),
        };
        let tags = tags?;
        finished?;
        Ok(tags)
    }

    /// The encoders' last packets, sent and written.
    fn flush_all(&mut self) -> Result<Vec<Tag>, AvError> {
        // The sound given so far first: the video's last packets may start a file or a stream
        // (as `video` settles before a frame that could).
        self.settle_audio()?;
        let packets = self.video.flush()?;
        let mut tags = std::mem::take(&mut self.carry);
        tags.extend(self.video_out(packets)?);
        let offset = self.audio_offset_us.unwrap_or(0);
        let last = self.audio.flush()?;
        if !last.is_empty() && !self.sent_audio_header && !self.stream_waits_key {
            self.sent_audio_header = true;
            tags.push(Tag::Audio { ms: 0, body: flv::audio_header(&self.audio.header()) });
        }
        for mut p in last {
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
        let start_us = self.stream_from_us;
        for i in 0..self.video_tracks.len() {
            let (enc, sent) = &mut self.video_tracks[i];
            let packets = enc.flush()?;
            tags.extend(Self::track_tags(i, enc, sent, packets, start_us));
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
        matches!(self.recording, Some(Recording::Failed(_)))
    }

    /// Why the recording stopped, when it did.
    pub fn recording_problem(&self) -> Option<&RecordProblem> {
        match &self.recording {
            Some(Recording::Failed(p)) => Some(p),
            _ => None,
        }
    }

    /// A time on the output's clock (µs) as the stream's tag time (ms).
    fn stream_ms(&self, us: i64) -> u32 {
        ((us - self.stream_from_us).max(0) / 1000) as u32
    }

    /// Nominal output frame duration in microseconds (at least 1).
    fn frame_us(&self) -> i64 {
        let vcfg = *self.video.config();
        (1_000_000 * i64::from(vcfg.fps_den.max(1)) / i64::from(vcfg.fps_num.max(1))).max(1)
    }

    fn video_out(&mut self, packets: Vec<VideoPacket>) -> Result<Vec<Tag>, AvError> {
        let codec = self.video.config().codec;
        let frame_us = self.frame_us();
        let max_step = (frame_us / 4).max(1);
        let mut tags = Vec::new();
        for mut p in packets {
            if let Some(last) = self.last_video_dts_us
                && p.dts_us <= last
            {
                let clamped = monotonic_dts_step(last, p.pts_us, max_step);
                if !self.dts_clamp_logged {
                    self.dts_clamp_logged = true;
                    tracing::warn!(
                        pts_us = p.pts_us,
                        dts_us = p.dts_us,
                        clamped_dts_us = clamped,
                        last_dts_us = last,
                        "pipeline clamped video decode timestamp to stay monotonic"
                    );
                }
                p.dts_us = clamped;
            }
            self.last_video_dts_us = Some(p.dts_us);
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
        let frame_us = self.frame_us();
        if let Some(c) = self.rec_cuts.last_mut()
            && c.to.is_none()
        {
            match cut_step(c.from, self.rec_resume, p.key, p.pts_us, self.rec_cut_entered) {
                CutStep::Write => {}
                CutStep::Leave => {
                    self.rec_cut_entered = true;
                    return;
                }
                CutStep::End => {
                    let resume_at =
                        self.rec_last_raw_pts_us.map_or(c.from, |lp| (lp + frame_us).max(c.from)).min(p.pts_us);
                    c.to = Some(p.pts_us);
                    c.cut = p.pts_us - resume_at;
                    self.rec_resume = None;
                    self.rec_cut_entered = false;
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
        if self.split_wanted {
            if self.split_to.is_none() {
                // Held for the new file (a few seconds at most): the old one ends at the last
                // frame the lost session made.
                if self.split_held.len() < SPLIT_HELD_MAX {
                    self.split_held.push_back(p.clone());
                    return;
                }
                tracing::warn!("no new file came for the rebuilt encoder; the recording goes on in this one");
                self.split_wanted = false;
                for h in std::mem::take(&mut self.split_held) {
                    self.write_video(&h);
                }
            } else {
                self.split_wanted = false;
                self.auto_splits += 1;
                for h in std::mem::take(&mut self.split_held) {
                    self.write_video(&h);
                }
            }
        }
        let raw_pts = p.pts_us;
        self.rec_last_raw_pts_us = Some(self.rec_last_raw_pts_us.map_or(raw_pts, |prev| prev.max(raw_pts)));
        // The recording alone paused: frames in a cut aren't written, later ones move back.
        let mut shifted;
        let frame_us = self.frame_us();
        let p = if self.rec_cuts.is_empty() {
            p
        } else {
            let Some(at) = self.rec_time(p.pts_us) else { return };
            let by = p.pts_us - at;
            shifted = VideoPacket { pts_us: at, dts_us: p.dts_us - by, ..p.clone() };
            if let Some(last) = self.rec_last_dts_us
                && shifted.dts_us <= last
            {
                shifted.dts_us = monotonic_dts_step(last, shifted.pts_us, (frame_us / 4).max(1));
            }
            &shifted
        };
        self.rec_last_dts_us = Some(p.dts_us);
        let header = self.video.header();
        let cfg = *self.video.config();
        let acfg = *self.audio.config();
        let aheader = self.audio.header();
        // A recording that failed isn't tried again, and no new file is made for it (a split
        // that asked for one before it failed is let go).
        if matches!(self.recording, Some(Recording::Failed(_))) {
            self.drop_pending_file();
            return;
        }
        // A split waiting: the old file is closed and the new one starts on this keyframe.
        if p.key
            && let Some(next) = self.split_to.take()
        {
            if let Some(Recording::On(w)) = self.recording.take()
                && let Err(u) = w.finish()
            {
                let e = u.cut();
                let problem = RecordProblem::from_io(&e);
                tracing::error!(error = %e, reason = %problem.reason, "the recording couldn't be finished at a split");
                self.recording = Some(Recording::Failed(problem));
                Self::forget_empty(&next.2);
                return;
            }
            self.recording =
                Some(Recording::Waiting { file: BufWriter::new(next.0), kind: next.1, held: VecDeque::new() });
        }
        let rec = match self.recording.take() {
            Some(Recording::Waiting { file, kind, held }) => match header {
                Some(config) => {
                    let v = VideoTrack { codec: cfg.codec, width: cfg.width, height: cfg.height, config };
                    let a = AudioTrack {
                        sample_rate: acfg.sample_rate,
                        channels: acfg.channels,
                        config: aheader,
                        bitrate_kbps: acfg.bitrate_kbps,
                        name: String::new(),
                    };
                    // Every track the same shape (the extra encoders share the first's settings),
                    // each with the name the streamer gave it.
                    let tracks: Vec<AudioTrack> = (0..1 + self.extra.len())
                        .map(|i| AudioTrack { name: self.track_names.get(i).cloned().unwrap_or_default(), ..a.clone() })
                        .collect();
                    match Writer::new(file, kind, v, tracks) {
                        Ok(mut w) => {
                            let wrote = w.video(p).and_then(|()| held.iter().try_for_each(|(t, a)| w.audio(*t, a)));
                            match wrote {
                                Ok(()) => Recording::On(Box::new(w)),
                                Err(e) => Recording::Failed(give_up(Box::new(w), &e)),
                            }
                        }
                        Err(e) => {
                            let problem = RecordProblem::from_io(&e);
                            tracing::error!(error = %e, reason = %problem.reason, "the recording couldn't be started");
                            Recording::Failed(problem)
                        }
                    }
                }
                None => Recording::Waiting { file, kind, held },
            },
            Some(Recording::On(mut w)) => match w.video(p) {
                Ok(()) => Recording::On(w),
                Err(e) => Recording::Failed(give_up(w, &e)),
            },
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
            if self.rec_resume.is_some() && self.rec_cuts.last().is_some_and(|c| c.to.is_none() && p.pts_us >= c.from) {
                push_held_audio(&mut self.rec_held, (track, p));
                return;
            }
            let Some(at) = self.rec_time(p.pts_us) else { return };
            p.pts_us = at;
        }
        match &mut self.recording {
            Some(Recording::Waiting { held, .. }) => push_held_audio(held, (track, p)),
            Some(Recording::On(_)) => {
                let Some(Recording::On(mut w)) = self.recording.take() else { return };
                self.recording = Some(match w.audio(track, &p) {
                    Ok(()) => Recording::On(w),
                    Err(e) => Recording::Failed(give_up(w, &e)),
                });
            }
            _ => {}
        }
    }
}

/// One paused interval excluded from a recording sharing its encoder with a stream.
#[derive(Debug, Clone, Copy)]
struct RecCut {
    from: i64,
    to: Option<i64>,
    cut: i64,
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

/// The most video packets held while a recording waits to be told its next file.
const SPLIT_HELD_MAX: usize = 600;

fn cut_step(from: i64, resume: Option<i64>, key: bool, pts: i64, entered: bool) -> CutStep {
    match resume {
        _ if !entered && pts < from => CutStep::Write,
        Some(at) if key && pts >= at => CutStep::End,
        _ => CutStep::Leave,
    }
}

impl Pipeline {
    /// Tests: how many frames the encoder has been handed.
    #[cfg(test)]
    pub(crate) fn video_calls_for_test(&self) -> u64 {
        self.video_calls
    }

    /// Tests: the hardware session is taken away.
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn lose_video_session_for_test(&mut self) {
        self.video.lose_session_for_test(false);
    }

    /// Tests: the next video frame fails, once.
    #[cfg(test)]
    pub(crate) fn fail_video_once_for_test(&mut self) {
        self.fail_video_once = true;
    }

    /// Tests: this output's encoder faults on every frame from now on.
    #[cfg(test)]
    pub(crate) fn fail_video_for_test(&mut self) {
        self.fail_video = true;
    }

    /// Records into `file` (tests: a file that fails when it is told to).
    #[cfg(test)]
    fn record_into(&mut self, file: File) {
        self.recording =
            Some(Recording::Waiting { file: BufWriter::new(file), kind: Kind::Mp4, held: VecDeque::new() });
    }
}

/// Last-resort DTS bump that rises above `last` while leaving headroom before `pts_us` for buffered B-frames.
fn monotonic_dts_step(last: i64, pts_us: i64, max_step: i64) -> i64 {
    if pts_us > last {
        let headroom = pts_us - last;
        last + (headroom / 4).clamp(1, max_step.max(1)).min(headroom)
    } else {
        last + 1
    }
}

/// A disk that runs out (M7-01): the recording ends, what was written plays, it is said once,
/// and nothing is retried.
#[cfg(test)]
mod full_disk_tests {
    use std::io::{self, Read, Write};

    use super::*;
    use crate::mp4::{AudioTrack, Mp4Writer, VideoTrack};
    use crate::video::{PixelFormat, RateControl, VideoCodec};

    /// A sink that takes `room` bytes and then says the disk is full (a short write first, as a
    /// disk does when it has less room than the write).
    struct Disk {
        got: Vec<u8>,
        room: usize,
    }

    impl Write for Disk {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.got.len() >= self.room {
                return Err(io::Error::from(io::ErrorKind::StorageFull));
            }
            let n = buf.len().min(self.room - self.got.len());
            self.got.extend_from_slice(&buf[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
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

    /// Feeds 200 packets (a keyframe every 10) of about 1 KB each.
    fn feed(w: &mut Mp4Writer<Disk>) -> io::Result<()> {
        for i in 0..200i64 {
            let pts = i * 33_333;
            w.video(&VideoPacket { pts_us: pts, dts_us: pts, key: i % 10 == 0, data: vec![i as u8; 1000] })?;
            w.audio(&AudioPacket { pts_us: pts, data: vec![7; 40] })?;
        }
        Ok(())
    }

    /// The top-level boxes of an MP4 as (kind, size), and whether they end exactly at its end.
    fn boxes(b: &[u8]) -> (Vec<([u8; 4], usize)>, bool) {
        let (mut at, mut out) = (0usize, Vec::new());
        while at + 8 <= b.len() {
            let size = u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as usize;
            let kind = [b[at + 4], b[at + 5], b[at + 6], b[at + 7]];
            if size < 8 || at + size > b.len() {
                return (out, false);
            }
            out.push((kind, size));
            at += size;
        }
        (out, at == b.len())
    }

    #[test]
    fn a_disk_that_fills_leaves_a_file_that_ends_on_a_whole_fragment() {
        let (v, a) = tracks();
        // Room for the header and a few fragments, not for all of them.
        let mut w =
            Mp4Writer::new(Disk { got: Vec::new(), room: 40_000 }, Some(v), Some(a)).unwrap_or_else(|e| panic!("{e}"));
        let err = feed(&mut w).expect_err("the disk is full");
        assert_eq!(err.kind(), io::ErrorKind::StorageFull);
        let (mut disk, whole) = w.abandon();
        // Before it is cut back, the file ends in the middle of a fragment…
        assert!(!boxes(&disk.got).1, "a piece was cut short");
        assert!(whole < disk.got.len() as u64);
        // …after, every box is whole and there are fragments to play.
        disk.got.truncate(whole as usize);
        let (list, clean) = boxes(&disk.got);
        assert!(clean, "{list:?}");
        assert!(list.iter().filter(|(k, _)| k == b"moof").count() >= 2, "{list:?}");
        assert_eq!(list[0].0, *b"ftyp");
        assert_eq!(list.last().map(|l| l.0), Some(*b"mdat"));
    }

    #[test]
    fn written_counts_a_fragment_only_once_it_is_all_out() {
        let (v, a) = tracks();
        let mut w =
            Mp4Writer::new(Disk { got: Vec::new(), room: 40_000 }, Some(v), Some(a)).unwrap_or_else(|e| panic!("{e}"));
        let _ = feed(&mut w);
        let (disk, whole) = w.abandon();
        assert!(whole <= disk.got.len() as u64, "never past what is in the file: {whole} of {}", disk.got.len());
    }

    #[test]
    fn salvage_cuts_a_file_back_and_never_makes_it_longer() {
        let dir = std::env::temp_dir().join(format!("ferrocore-salvage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
        let path = dir.join("x.mp4");
        let mut f = BufWriter::new(File::create(&path).unwrap_or_else(|e| panic!("{e}")));
        f.write_all(&[1; 1000]).unwrap_or_else(|e| panic!("{e}"));
        f.flush().unwrap_or_else(|e| panic!("{e}"));
        f.write_all(&[2; 50]).unwrap_or_else(|e| panic!("{e}")); // still in the buffer
        salvage(f, 600);
        assert_eq!(std::fs::metadata(&path).map(|m| m.len()).ok(), Some(600));
        // A count that is past the end (a flush that failed) leaves the file as it is.
        let mut f = BufWriter::new(File::create(&path).unwrap_or_else(|e| panic!("{e}")));
        f.write_all(&[1; 100]).unwrap_or_else(|e| panic!("{e}"));
        f.flush().unwrap_or_else(|e| panic!("{e}"));
        salvage(f, 5_000);
        assert_eq!(std::fs::metadata(&path).map(|m| m.len()).ok(), Some(100));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_went_wrong_is_said_in_words() {
        let say = |e: io::Error| RecordProblem::from_io(&e);
        for e in [
            io::Error::from(io::ErrorKind::StorageFull),
            io::Error::from_raw_os_error(if cfg!(windows) { 112 } else { 28 }),
        ] {
            let p = say(e);
            assert_eq!(p.reason, "the disk is full");
            assert!(p.detail.contains("Free some space") && p.detail.contains("plays"), "{p:?}");
        }
        assert_eq!(say(io::Error::from(io::ErrorKind::NotFound)).reason, "the drive isn't there anymore");
        assert_eq!(say(io::Error::from(io::ErrorKind::PermissionDenied)).reason, "the folder can't be written");
        assert_eq!(say(io::Error::other("odd")).reason, "the file couldn't be written");
        assert!(say(io::Error::other("x".repeat(1000))).detail.len() < 400, "long system text is cut");
    }

    /// Whether this computer encodes (the Mac always; elsewhere with FFmpeg's H.264).
    fn encodes() -> bool {
        (cfg!(target_os = "macos") && std::env::var("FERROCAST_AV_ENCODER").as_deref() != Ok("ffmpeg"))
            || crate::ffenc::h264_encoder().is_some()
    }

    /// The whole pipeline writing into a file whose other end goes away partway (a drive taken
    /// out): the recording fails once, the file so far is whole, and later frames, a split and
    /// more frames do nothing more.
    #[cfg(unix)]
    #[test]
    fn a_recording_that_cannot_be_written_stops_once_and_is_not_tried_again() {
        use std::os::unix::net::UnixStream;
        if !encodes() {
            return;
        }
        let video = VideoConfig {
            codec: VideoCodec::H264,
            input: PixelFormat::Nv12,
            width: 320,
            height: 180,
            fps_num: 30,
            fps_den: 1,
            bitrate_kbps: 1500,
            rate: RateControl::Cbr,
            keyframe_secs: 1,
            software: false,
            profile: crate::video::H264Profile::High,
            bframes: false,
            preset: None,
            tune: None,
        };
        let mut pipe = Pipeline::new(video, AudioConfig { sample_rate: 48000, channels: 2, bitrate_kbps: 128 }, None)
            .unwrap_or_else(|e| panic!("{e}"));
        let (ours, mut theirs) = UnixStream::pair().unwrap_or_else(|e| panic!("{e}"));
        // The far end takes 30 KB and goes away.
        let reader = std::thread::spawn(move || {
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            while got.len() < 30_000 {
                match theirs.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                }
            }
            got
        });
        pipe.record_into(File::from(std::os::fd::OwnedFd::from(ours)));
        let (y, uv) = (vec![90u8; 320 * 180], vec![128u8; 320 * 90]);
        let mut failed_at = None;
        for i in 0..400u64 {
            let ts = i * 33_333_333;
            let _ = pipe.video(Frame::Nv12 { y: &y, y_stride: 320, uv: &uv, uv_stride: 320 }, ts);
            let _ = pipe.audio(&vec![0.1f32; 1600 * 2], ts);
            if failed_at.is_none() && pipe.recording_failed() {
                failed_at = Some(i);
            }
            if failed_at.is_some_and(|f| i > f + 40) {
                break;
            }
        }
        let got = reader.join().unwrap_or_default();
        let failed_at = failed_at.expect("the recording failed once the far end was gone");
        assert!(failed_at > 0, "not at the first frame: it was written until the end went away");
        let p = pipe.recording_problem().expect("a reason").clone();
        assert_eq!(p.reason, "the drive isn't there anymore", "{p:?}");
        assert!(!pipe.recording(), "it no longer says it records");
        assert!(pipe.recorded_bytes() > 0, "the size of what was kept goes on showing");
        // Frames after it write nothing and don't change the answer; a split is refused in words.
        let again = pipe.recording_problem().cloned();
        assert_eq!(again, Some(p));
        let err = pipe.split(Path::new("/nonexistent/next.mp4")).expect_err("no split of a failed recording");
        assert!(err.to_string().contains("the recording stopped: the drive isn't there anymore"), "{err}");
        // What the far end got starts like an MP4 (the header went out whole).
        assert_eq!(&got[4..8], b"ftyp");
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
        assert_eq!(cut_step(from, None, false, 2_966_667, false), CutStep::Write);
        assert_eq!(cut_step(from, Some(at), false, 2_966_667, false), CutStep::Write);
        assert_eq!(cut_step(from, None, false, 2_966_667, true), CutStep::Leave, "reordered B-frame after dropped P");
        assert_eq!(cut_step(from, None, true, 3_500_000, false), CutStep::Leave, "still paused");
        assert_eq!(cut_step(from, Some(at), true, 3_800_000, false), CutStep::Leave, "a keyframe drawn while paused");
        assert_eq!(cut_step(from, Some(at), false, 3_933_333, true), CutStep::Leave, "not a keyframe");
        assert_eq!(cut_step(from, Some(at), true, 3_900_000, true), CutStep::End);
        assert_eq!(cut_step(from, Some(at), true, 4_000_000, true), CutStep::End);
    }

    #[test]
    fn replay_audio_and_held_queues_stay_bounded_if_video_stalls() {
        let mut r = super::Replay {
            keep_us: 30_000_000,
            max_bytes: 64 * 1024,
            video: super::VecDeque::new(),
            audio: super::VecDeque::new(),
            bytes: 0,
        };
        // One video keyframe arrives, then video stalls completely while 100k audio packets arrive.
        r.video(&super::VideoPacket { pts_us: 0, dts_us: 0, key: true, data: vec![0u8; 1024] });
        for i in 0..100_000i64 {
            r.audio(0, &super::AudioPacket { pts_us: i * 21_333, data: vec![0u8; 256] });
        }
        assert!(r.bytes <= r.max_bytes, "replay bytes stayed capped: {} > {}", r.bytes, r.max_bytes);
        assert!(r.audio.len() <= 300, "replay audio count stayed bounded: {}", r.audio.len());

        // Time-only bound (max_bytes = 0): still bounded by keep_us + 2 s margin.
        let mut r_time = super::Replay {
            keep_us: 10_000_000,
            max_bytes: 0,
            video: super::VecDeque::new(),
            audio: super::VecDeque::new(),
            bytes: 0,
        };
        r_time.video(&super::VideoPacket { pts_us: 0, dts_us: 0, key: true, data: vec![0u8; 1024] });
        for i in 0..50_000i64 {
            r_time.audio(0, &super::AudioPacket { pts_us: i * 21_333, data: vec![0u8; 128] });
        }
        let span = r_time.audio.back().unwrap().1.pts_us - r_time.audio.front().unwrap().1.pts_us;
        assert!(span <= 12_050_000, "replay audio span bounded by keep_us + 2s: {span}");

        // Held audio queue cap when waiting for video header / post-resume keyframe.
        let mut held = super::VecDeque::new();
        for i in 0..10_000i64 {
            super::push_held_audio(&mut held, (0, super::AudioPacket { pts_us: i * 21_333, data: vec![1u8; 64] }));
        }
        assert_eq!(held.len(), super::MAX_HELD_AUDIO);
        assert_eq!(held.back().unwrap().1.pts_us, 9_999 * 21_333);
    }

    #[test]
    fn recording_pause_with_bframes_never_orphans_bframes_or_moves_dts_backwards() {
        // Mini-GOP (P=base+66.7ms, B=base, B=base+33.3ms) straddling a pause cut at
        // from=base+33.3ms+1, followed by a resume keyframe at pts=base+2s, dts=base+1.9s.
        let pipe = super::Pipeline::new(
            crate::video::VideoConfig {
                codec: crate::video::VideoCodec::H264,
                input: crate::video::PixelFormat::Nv12,
                width: 64,
                height: 64,
                fps_num: 30,
                fps_den: 1,
                bitrate_kbps: 1500,
                rate: crate::video::RateControl::Cbr,
                keyframe_secs: 1,
                software: false,
                profile: crate::video::H264Profile::High,
                bframes: true,
                preset: None,
                tune: None,
            },
            crate::audio::AudioConfig { sample_rate: 48000, channels: 2, bitrate_kbps: 128 },
            None,
        );
        let Ok(mut pipe) = pipe else { return };
        let dir = std::env::temp_dir().join(format!("ferrocore-rec-cut-bf-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("cut.mp4");
        pipe.start_recording(&path).unwrap_or_else(|e| panic!("{e}"));

        // Feed frames until the async encoder has produced its codec header and written at least
        // one keyframe into `Recording::On` (bounded condition wait; never assume a fixed count).
        let (y, uv) = (vec![90u8; 64 * 64], vec![128u8; 64 * 32]);
        let frame = crate::video::Frame::Nv12 { y: &y, y_stride: 64, uv: &uv, uv_stride: 64 };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut i = 0u64;
        loop {
            let ts = i * 33_333_333;
            i += 1;
            pipe.video(frame, ts).unwrap_or_else(|e| panic!("{e}"));
            assert!(!pipe.recording_failed(), "recording failed during startup: {:?}", pipe.recording_problem());
            if pipe.video.header().is_some()
                && pipe.rec_last_dts_us.is_some()
                && matches!(pipe.recording, Some(super::Recording::On(_)))
            {
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = std::fs::remove_dir_all(&dir);
                panic!(
                    "encoder did not start recording before pause within 5s (header={}, dts={:?})",
                    pipe.video.header().is_some(),
                    pipe.rec_last_dts_us,
                );
            }
            let wait_ms = if i < 10 { 2 } else { 10 };
            std::thread::sleep(std::time::Duration::from_millis(wait_ms));
        }

        let Some(dts_before_pause) = pipe.rec_last_dts_us else {
            panic!("wrote frames before pause");
        };
        let base = pipe.last_in_us.max(pipe.rec_last_raw_pts_us.unwrap_or(0)).max(dts_before_pause) + 100_000;
        // Distinct 16-byte ASCII tags avoid colliding with encoded H.264 bytes or MP4 sample tables.
        let pkt = |pts: i64, dts: i64, key: bool, tag: &[u8; 16]| {
            let mut data = vec![0, 0, 0, 16];
            data.extend_from_slice(tag);
            super::VideoPacket { pts_us: pts, dts_us: dts, key, data }
        };
        const TAG_PRE_P: &[u8; 16] = b"FCUT_PRE_P_00001";
        const TAG_PRE_B: &[u8; 16] = b"FCUT_PRE_B_00002";
        const TAG_PAU_P: &[u8; 16] = b"FCUT_PAU_P_00003";
        const TAG_PAU_B1: &[u8; 16] = b"FCUT_PAU_B_00004";
        const TAG_PAU_B2: &[u8; 16] = b"FCUT_PAU_B_00005";
        const TAG_RES_K: &[u8; 16] = b"FCUT_RES_K_00006";
        const TAG_RES_P: &[u8; 16] = b"FCUT_RES_P_00007";

        // Pause at from = base + 33_334. A pre-pause mini-GOP still in the encoder (pts < from)
        // must still be written in decode order before the cut enters.
        pipe.last_in_us = base + 33_333;
        pipe.pause_recording(true);
        pipe.record_video(&pkt(base - 33_333, base - 66_667, false, TAG_PRE_P));
        pipe.record_video(&pkt(base - 66_667, base - 33_333, false, TAG_PRE_B));
        let Some(dts_before_cut) = pipe.rec_last_dts_us else {
            panic!("pre-pause mini-GOP wrote frames");
        };

        // Simulate decode-order packets straddling the pause cut:
        // P(pts=base+66.7ms) arrives FIRST (dropped, enters cut), followed by B(base) and
        // B(base+33.3ms) with pts < from (must also be dropped, never orphaned).
        pipe.record_video(&pkt(base + 66_667, base, false, TAG_PAU_P));
        pipe.record_video(&pkt(base, base + 33_333, false, TAG_PAU_B1));
        pipe.record_video(&pkt(base + 33_333, base + 66_667, false, TAG_PAU_B2));
        pipe.pause_recording(false); // rec_resume = Some(base + 33_334)
        pipe.record_video(&pkt(base + 2_000_000, base + 1_900_000, true, TAG_RES_K));
        let Some(dts_after_resume) = pipe.rec_last_dts_us else {
            panic!("wrote resume keyframe");
        };
        assert!(
            dts_after_resume > dts_before_cut,
            "recording dts must not move backwards across resume: before={dts_before_cut} after={dts_after_resume}"
        );
        pipe.record_video(&pkt(base + 2_100_000, base + 1_933_333, false, TAG_RES_P));
        let Some(dts_next) = pipe.rec_last_dts_us else {
            panic!("wrote post-resume frame");
        };
        assert!(dts_next > dts_after_resume, "dts continues rising: {dts_after_resume} -> {dts_next}");
        pipe.end_recording().unwrap_or_else(|e| panic!("{e}"));
        let bytes = std::fs::read(&path).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        let has = |tag: &[u8; 16]| bytes.windows(16).any(|w| w == tag);
        assert!(has(TAG_PRE_P) && has(TAG_PRE_B), "pre-pause buffered mini-GOP was written");
        let (has_p, has_b1, has_b2) = (has(TAG_PAU_P), has(TAG_PAU_B1), has(TAG_PAU_B2));
        assert!(
            !has_p && !has_b1 && !has_b2,
            "straddling mini-GOP dropped without orphaning B-frames (p={has_p}, b1={has_b1}, b2={has_b2})"
        );
        assert!(has(TAG_RES_K) && has(TAG_RES_P), "resumed keyframe and following frame were written");
    }
}

/// A hardware session lost and rebuilt (GAPS #4): viewers' decoders hold the first SPS/PPS,
/// so a different pair needs a new sequence header before the keyframe; the same needs none.
#[cfg(all(test, target_os = "macos"))]
mod rebuild_tests {
    use super::*;
    use crate::video::{PixelFormat, RateControl, VideoCodec};

    fn sequence_headers(tags: &[Tag]) -> usize {
        tags.iter().filter(|t| matches!(t, Tag::Video { body, .. } if body[0] & 0x0f == 7 && body[1] == 0)).count()
    }

    fn run(differ: bool) -> Option<(usize, Vec<Tag>)> {
        let video = VideoConfig {
            codec: VideoCodec::H264,
            input: PixelFormat::Nv12,
            width: 320,
            height: 180,
            fps_num: 30,
            fps_den: 1,
            bitrate_kbps: 1500,
            rate: RateControl::Cbr,
            keyframe_secs: 2,
            software: false,
            profile: crate::video::H264Profile::High,
            bframes: false,
            preset: None,
            tune: None,
        };
        let mut pipe =
            Pipeline::new(video, AudioConfig { sample_rate: 48000, channels: 2, bitrate_kbps: 128 }, None).ok()?;
        let (y, uv) = (vec![90u8; 320 * 180], vec![128u8; 320 * 90]);
        let frame = || Frame::Nv12 { y: &y, y_stride: 320, uv: &uv, uv_stride: 320 };
        let mut tags = Vec::new();
        for i in 0..10u64 {
            tags.extend(pipe.video(frame(), i * 33_333_333).ok()?);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let before = sequence_headers(&tags);
        pipe.video.lose_session_for_test(differ);
        for i in 10..60u64 {
            tags.extend(pipe.video(frame(), i * 33_333_333).ok()?);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Some((before, tags))
    }

    #[test]
    fn a_rebuilt_session_with_other_parameter_sets_sends_a_new_sequence_header_first() {
        let Some((before, tags)) = run(true) else { return };
        assert_eq!(before, 1, "the first header went out at the start");
        assert_eq!(sequence_headers(&tags), 2, "a new one for the rebuilt session");
        // It comes before the keyframe that follows the rebuild.
        let at = tags
            .iter()
            .rposition(|t| matches!(t, Tag::Video { body, .. } if body[0] & 0x0f == 7 && body[1] == 0))
            .unwrap_or(0);
        assert!(
            matches!(tags.get(at + 1), Some(Tag::Video { body, .. }) if body[0] >> 4 == 1 && body[1] == 1),
            "a keyframe follows the new header"
        );
    }

    fn have(tool: &str) -> bool {
        std::process::Command::new(tool).arg("-version").output().is_ok_and(|o| o.status.success())
    }

    /// Records `frames` frames, losing the hardware session half way; the "studio tick" names the
    /// next file when the pipeline asks. Returns the files made.
    fn record(differ: bool) -> Option<(Vec<std::path::PathBuf>, u32)> {
        let video = VideoConfig {
            codec: VideoCodec::H264,
            input: PixelFormat::Nv12,
            width: 320,
            height: 180,
            fps_num: 30,
            fps_den: 1,
            bitrate_kbps: 1500,
            rate: RateControl::Cbr,
            keyframe_secs: 2,
            software: false,
            profile: crate::video::H264Profile::High,
            bframes: false,
            preset: None,
            tune: None,
        };
        let dir = std::env::temp_dir().join(format!("ferrocore-rebuild-{}-{differ}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        let (one, two) = (dir.join("part1.mp4"), dir.join("part2.mp4"));
        let mut pipe =
            Pipeline::new(video, AudioConfig { sample_rate: 48000, channels: 2, bitrate_kbps: 128 }, Some(&one))
                .ok()?;
        let (y, uv) = (vec![90u8; 320 * 180], vec![128u8; 320 * 90]);
        let pcm = vec![0.0f32; 1600 * 2];
        for i in 0..90u64 {
            let ts = i * 33_333_333;
            pipe.video(Frame::Nv12 { y: &y, y_stride: 320, uv: &uv, uv_stride: 320 }, ts).ok()?;
            pipe.audio(&pcm, ts).ok()?;
            if i == 40 {
                pipe.video.lose_session_for_test(differ);
            }
            // The studio's tick: a new file when the pipeline asks for one.
            if pipe.split_wanted() && !pipe.splitting() {
                pipe.split(&two).ok()?;
            }
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
        let splits = pipe.auto_splits();
        pipe.finish().ok()?;
        let mut files = vec![one];
        if two.exists() && std::fs::metadata(&two).map_or(0, |m| m.len()) > 0 {
            files.push(two);
        }
        Some((files, splits))
    }

    /// Each file plays on its own: it decodes without an error, and its first video packet is a
    /// keyframe (one avcC per file, so a different parameter set means a different file).
    fn plays_alone(f: &std::path::Path) -> bool {
        let dec =
            std::process::Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(f).args(["-f", "null", "-"]).output();
        let first = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=flags", "-of", "csv=p=0"])
            .arg(f)
            .output();
        let (Ok(dec), Ok(first)) = (dec, first) else { return false };
        let flags = String::from_utf8_lossy(&first.stdout);
        dec.stderr.is_empty() && flags.lines().next().is_some_and(|l| l.contains('K'))
    }

    #[test]
    fn a_recording_goes_on_in_a_new_file_when_the_parameter_sets_change() {
        if !have("ffmpeg") || !have("ffprobe") {
            return;
        }
        let Some((files, splits)) = record(true) else { return };
        assert_eq!((files.len(), splits), (2, 1), "{files:?}");
        for f in &files {
            assert!(plays_alone(f), "{f:?} must decode on its own and start on a keyframe");
        }
    }

    #[test]
    fn a_recording_stays_one_file_when_the_parameter_sets_are_the_same() {
        if !have("ffmpeg") || !have("ffprobe") {
            return;
        }
        let Some((files, splits)) = record(false) else { return };
        assert_eq!((files.len(), splits), (1, 0), "{files:?}");
        assert!(plays_alone(&files[0]));
    }

    /// A quality of Twitch's Enhanced Broadcasting is its own hardware session: when it is
    /// rebuilt with other parameter sets its new header goes out before its keyframe.
    #[test]
    fn an_extra_quality_rebuilt_with_other_parameter_sets_sends_its_own_new_header() {
        let video = VideoConfig {
            codec: VideoCodec::H264,
            input: PixelFormat::Nv12,
            width: 320,
            height: 180,
            fps_num: 30,
            fps_den: 1,
            bitrate_kbps: 1500,
            rate: RateControl::Cbr,
            keyframe_secs: 2,
            software: false,
            profile: crate::video::H264Profile::High,
            bframes: false,
            preset: None,
            tune: None,
        };
        let Ok(mut pipe) =
            Pipeline::new(video, AudioConfig { sample_rate: 48000, channels: 2, bitrate_kbps: 128 }, None)
        else {
            return;
        };
        if pipe.add_video_tracks(&[VideoConfig { width: 160, height: 90, bitrate_kbps: 500, ..video }]) != 1 {
            return;
        }
        let (y, uv) = (vec![90u8; 320 * 180], vec![128u8; 320 * 90]);
        let (y2, uv2) = (vec![90u8; 160 * 90], vec![128u8; 160 * 45]);
        let mut tags = Vec::new();
        for i in 0..60u64 {
            let ts = i * 33_333_333;
            pipe.video(Frame::Nv12 { y: &y, y_stride: 320, uv: &uv, uv_stride: 320 }, ts).ok();
            if let Ok(t) = pipe.video_track(0, Frame::Nv12 { y: &y2, y_stride: 160, uv: &uv2, uv_stride: 160 }, ts) {
                tags.extend(t);
            }
            if i == 20 {
                pipe.video_tracks[0].0.lose_session_for_test(true);
            }
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
        // A multitrack sequence header: packet type 6 (multitrack) in the first byte, 0 after it.
        let headers = tags
            .iter()
            .filter(|t| matches!(t, Tag::Video { body, .. } if body.len() > 1 && body[0] & 0x0f == 6 && body[1] == 0))
            .count();
        assert_eq!(headers, 2, "the quality's first header and the new one");
    }

    #[test]
    fn a_rebuilt_session_with_the_same_parameter_sets_sends_no_extra_header() {
        let Some((_, tags)) = run(false) else { return };
        assert_eq!(sequence_headers(&tags), 1);
    }
}

/// B-frames through the whole output: real Mac encoder, the stream's tags, and a recording (MP4
/// and MKV) that FFmpeg itself reads back.
#[cfg(all(test, target_os = "macos"))]
mod bframe_tests {
    use super::*;
    use crate::video::{PixelFormat, RateControl, VideoCodec};

    fn have(tool: &str) -> bool {
        std::process::Command::new(tool).arg("-version").output().is_ok_and(|o| o.status.success())
    }

    fn video(bframes: bool) -> VideoConfig {
        VideoConfig {
            codec: VideoCodec::H264,
            input: PixelFormat::Nv12,
            width: 320,
            height: 180,
            fps_num: 30,
            fps_den: 1,
            bitrate_kbps: 1500,
            rate: RateControl::Cbr,
            keyframe_secs: 1,
            software: false,
            profile: crate::video::H264Profile::High,
            bframes,
            preset: None,
            tune: None,
        }
    }

    /// 150 moving frames with sound into `path`; the stream's video tags and the file.
    fn run(bframes: bool, path: &std::path::Path) -> Option<Vec<(u32, bool, i64)>> {
        let mut pipe = Pipeline::new(
            video(bframes),
            AudioConfig { sample_rate: 48000, channels: 2, bitrate_kbps: 128 },
            Some(path),
        )
        .ok()?;
        let pcm = vec![0.05f32; 1600 * 2];
        let mut tags = Vec::new();
        for i in 0..150u64 {
            let ts = i * 33_333_333;
            let y: Vec<u8> = (0..320 * 180).map(|p| ((p + i as usize * 7) % 251) as u8).collect();
            let uv = vec![128u8; 320 * 90];
            tags.extend(pipe.video(Frame::Nv12 { y: &y, y_stride: 320, uv: &uv, uv_stride: 320 }, ts).ok()?);
            pipe.audio(&pcm, ts).ok()?;
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        tags.extend(pipe.finish().ok()?);
        // (tag ms, keyframe, composition time ms) of every picture's tag.
        Some(
            tags.iter()
                .filter_map(|t| match t {
                    Tag::Video { ms, body } if body.len() > 5 && body[1] == 1 => {
                        Some((*ms, body[0] >> 4 == 1, i64::from(u32::from_be_bytes([0, body[2], body[3], body[4]]))))
                    }
                    _ => None,
                })
                .collect(),
        )
    }

    fn packets(f: &std::path::Path, stream: &str) -> Vec<(f64, f64)> {
        let o = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                stream,
                "-show_entries",
                "packet=pts_time,dts_time",
                "-of",
                "csv=p=0",
            ])
            .arg(f)
            .output()
            .unwrap_or_else(|e| panic!("{e}"));
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|l| {
                let (p, d) = l.split_once(',')?;
                Some((p.parse().ok()?, d.parse().ok()?))
            })
            .collect()
    }

    #[test]
    fn a_stream_and_recordings_with_b_frames_play_and_stay_in_sync() {
        if !have("ffmpeg") || !have("ffprobe") {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ferrocore-bframes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
        for ext in ["mp4", "mkv"] {
            let f = dir.join(format!("b.{ext}"));
            let Some(tags) = run(true, &f) else { return };
            // The stream: tag times (decode) never go back; some pictures carry a composition time.
            assert!(tags.windows(2).all(|w| w[1].0 >= w[0].0), "{ext}: tag times never go back");
            assert!(tags.iter().any(|t| t.2 > 0), "{ext}: B-frames have composition times");
            assert!(tags.first().is_some_and(|t| t.1 && t.0 == 0), "{ext}: starts on a keyframe at 0");
            // The file plays: FFmpeg decodes it without a complaint.
            let dec = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&f)
                .args(["-f", "null", "-"])
                .output()
                .unwrap_or_else(|e| panic!("{e}"));
            assert!(dec.stderr.is_empty(), "{ext}: {}", String::from_utf8_lossy(&dec.stderr));
            let v = packets(&f, "v:0");
            let a = packets(&f, "a:0");
            assert!(v.len() >= 140 && a.len() > 100, "{ext}: {} pictures, {} sound packets", v.len(), a.len());
            if ext == "mp4" {
                // MP4 keeps decode times: rising, never after the picture's time.
                assert!(v.windows(2).all(|w| w[1].1 > w[0].1), "{ext}: decode times rise");
                assert!(v.iter().all(|p| p.1 <= p.0 + 1e-6), "{ext}: decode never after display");
            }
            // Sync: the first picture shown and the first sound start within a frame of each other.
            let first_v = v.iter().map(|p| p.0).fold(f64::MAX, f64::min);
            let first_a = a.iter().map(|p| p.0).fold(f64::MAX, f64::min);
            assert!((first_v - first_a).abs() < 0.034 * 3.0, "{ext}: picture at {first_v}, sound at {first_a}");
            // The ends line up too (the file's two tracks are as long as each other).
            let last_v = v.iter().map(|p| p.0).fold(f64::MIN, f64::max);
            let last_a = a.iter().map(|p| p.0).fold(f64::MIN, f64::max);
            assert!((last_v - last_a).abs() < 0.2, "{ext}: ends at {last_v} and {last_a}");
        }
        // Without B-frames nothing is reordered (the old behaviour is unchanged).
        let f = dir.join("n.mp4");
        if let Some(tags) = run(false, &f) {
            assert!(tags.iter().all(|t| t.2 == 0), "no composition times without B-frames");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Stream sound tags (time, bytes) and each file's sound packet times.
    type SoundRun = (Vec<(u32, Vec<u8>)>, Vec<Vec<(f64, f64)>>);

    /// Sound through a pipeline with a pause, a recording-only pause and a split in it:
    /// (stream audio tags, then each file's sound packet times), with the AAC on or off the
    /// calling thread.
    fn sound_run(off_thread: bool, dir: &std::path::Path) -> Option<SoundRun> {
        let first = dir.join(format!("a{off_thread}.mp4"));
        let second = dir.join(format!("b{off_thread}.mp4"));
        let mut pipe = Pipeline::new(
            video(false),
            AudioConfig { sample_rate: 48000, channels: 2, bitrate_kbps: 128 },
            Some(&first),
        )
        .ok()?;
        if off_thread {
            pipe.audio_off_thread().ok()?;
        }
        // A tone that changes, so a block in the wrong place changes the bytes.
        let tone = |i: u64| -> Vec<f32> {
            (0..1600u64)
                .flat_map(|n| {
                    let v = (((i * 1600 + n) as f32) * 0.05).sin() * 0.3;
                    [v, -v]
                })
                .collect()
        };
        let mut tags = Vec::new();
        for i in 0..130u64 {
            let ts = i * 33_333_333;
            match i {
                30 => pipe.pause(true, ts),
                45 => pipe.pause(false, ts),
                60 => pipe.pause_recording(true),
                75 => pipe.pause_recording(false),
                100 => pipe.split(&second).ok()?,
                _ => {}
            }
            let y: Vec<u8> = (0..320 * 180).map(|p| ((p + i as usize * 7) % 251) as u8).collect();
            let uv = vec![128u8; 320 * 90];
            tags.extend(pipe.video(Frame::Nv12 { y: &y, y_stride: 320, uv: &uv, uv_stride: 320 }, ts).ok()?);
            tags.extend(pipe.audio(&tone(i), ts).ok()?);
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
        tags.extend(pipe.finish().ok()?);
        let audio = tags
            .into_iter()
            .filter_map(|t| match t {
                Tag::Audio { ms, body } => Some((ms, body)),
                _ => None,
            })
            .collect();
        let files = [first, second].iter().map(|f| packets(f, "a:0")).collect();
        Some((audio, files))
    }

    /// The AAC on a thread of its own gives the same sound packets, with the same times, on the
    /// stream and in both files, across a pause, a recording pause and a split.
    #[test]
    fn sound_encoded_off_the_calling_thread_is_the_same_sound() {
        if !have("ffprobe") {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ferrocore-aacthread-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
        let (Some(here), Some(there)) = (sound_run(false, &dir), sound_run(true, &dir)) else { return };
        assert!(here.0.len() > 100, "{} sound tags", here.0.len());
        assert_eq!(here.0.len(), there.0.len(), "same number of stream sound tags");
        assert!(here.0 == there.0, "stream sound tags identical (times and bytes)");
        assert_eq!(here.1.len(), there.1.len());
        for (i, (a, b)) in here.1.iter().zip(&there.1).enumerate() {
            assert!(!a.is_empty(), "file {i} has sound");
            assert_eq!(a, b, "file {i}: sound packet times identical");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Raw AAC tags' times (the sequence headers left out) from a stream with a `heal` in it.
    fn heal_run(off_thread: bool) -> Option<Vec<u32>> {
        let mut pipe =
            Pipeline::new(video(false), AudioConfig { sample_rate: 48000, channels: 2, bitrate_kbps: 128 }, None)
                .ok()?;
        if off_thread {
            pipe.audio_off_thread().ok()?;
        }
        let pcm = vec![0.05f32; 1600 * 2];
        let mut tags = Vec::new();
        for i in 0..90u64 {
            let ts = i * 33_333_333;
            if i == 45 {
                pipe.heal().ok()?;
            }
            let y: Vec<u8> = (0..320 * 180).map(|p| ((p + i as usize * 7) % 251) as u8).collect();
            let uv = vec![128u8; 320 * 90];
            tags.extend(pipe.video(Frame::Nv12 { y: &y, y_stride: 320, uv: &uv, uv_stride: 320 }, ts).ok()?);
            tags.extend(pipe.audio(&pcm, ts).ok()?);
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
        tags.extend(pipe.finish().ok()?);
        Some(
            tags.iter()
                .filter_map(|t| match t {
                    Tag::Audio { ms, body } if body.len() > 2 && body[1] == 1 => Some(*ms),
                    _ => None,
                })
                .collect(),
        )
    }

    /// After a rebuilt encoder the sound goes on at the output's own time: the new encoder counts
    /// from 0, so its start has to be found again (before, sound went out at time 0 again).
    #[test]
    fn healing_keeps_the_sound_on_the_outputs_clock() {
        for off_thread in [false, true] {
            let Some(ms) = heal_run(off_thread) else { return };
            assert!(ms.len() > 100, "{} sound tags", ms.len());
            assert!(ms.windows(2).all(|w| w[1] >= w[0]), "off_thread={off_thread}: sound times never go back");
            assert!(ms.last().is_some_and(|m| *m > 2500), "off_thread={off_thread}: sound runs to the end of the clip");
        }
    }

    /// A recording ended while sound is still being encoded has all of it: the same file as when
    /// the encoding is done in place.
    fn tail_run(off_thread: bool, dir: &std::path::Path) -> Option<Vec<(f64, f64)>> {
        let file = dir.join(format!("t{off_thread}.mp4"));
        let mut pipe = Pipeline::new(
            video(false),
            AudioConfig { sample_rate: 48000, channels: 2, bitrate_kbps: 128 },
            Some(&file),
        )
        .ok()?;
        if off_thread {
            pipe.audio_off_thread().ok()?;
        }
        let tone = |len: usize, seed: usize| -> Vec<f32> {
            (0..len)
                .flat_map(|n| {
                    let v = (((seed + n) as f32) * 0.05).sin() * 0.3;
                    [v, -v]
                })
                .collect()
        };
        for i in 0..40u64 {
            let ts = i * 33_333_333;
            let y: Vec<u8> = (0..320 * 180).map(|p| ((p + i as usize * 7) % 251) as u8).collect();
            let uv = vec![128u8; 320 * 90];
            pipe.video(Frame::Nv12 { y: &y, y_stride: 320, uv: &uv, uv_stride: 320 }, ts).ok()?;
            pipe.audio(&tone(1600, i as usize * 1600), ts).ok()?;
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
        // Two seconds of sound in one go, then the recording ends at once.
        pipe.audio(&tone(96_000, 99_999), 40 * 33_333_333).ok()?;
        pipe.end_recording().ok()?;
        pipe.finish().ok()?;
        Some(packets(&file, "a:0"))
    }

    #[test]
    fn ending_a_recording_keeps_the_sound_still_being_encoded() {
        if !have("ffprobe") {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ferrocore-aactail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
        let (Some(here), Some(there)) = (tail_run(false, &dir), tail_run(true, &dir)) else { return };
        assert!(here.len() > 100, "{} sound packets", here.len());
        assert_eq!(here, there, "the same sound packets, tail included");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bframe_stream_start_has_strictly_increasing_dts_and_non_negative_cts() {
        let mut pipe = match Pipeline::new(
            video(true),
            AudioConfig { sample_rate: 48000, channels: 2, bitrate_kbps: 128 },
            None,
        ) {
            Ok(p) => p,
            Err(_) => return,
        };
        pipe.add_video_tracks(&[VideoConfig { width: 320, height: 180, bitrate_kbps: 800, ..video(true) }]);
        let pcm = vec![0.05f32; 1600 * 2];
        let mut main_times = Vec::new();
        let mut track_times = Vec::new();
        let mut cts_list = Vec::new();
        for i in 0..45u64 {
            let ts = i * 33_333_333;
            let y: Vec<u8> = (0..320 * 180).map(|p| ((p + i as usize * 7) % 251) as u8).collect();
            let uv = vec![128u8; 320 * 90];
            let mut tags = pipe
                .video(Frame::Nv12 { y: &y, y_stride: 320, uv: &uv, uv_stride: 320 }, ts)
                .unwrap_or_else(|e| panic!("{e}"));
            tags.extend(
                pipe.video_track(0, Frame::Nv12 { y: &y, y_stride: 320, uv: &uv, uv_stride: 320 }, ts)
                    .unwrap_or_else(|e| panic!("{e}")),
            );
            let _ = pipe.audio(&pcm, ts);
            for t in tags {
                if let Tag::Video { ms, body } = t {
                    if body[0] & 0x80 != 0 && body[0] & 0x0F == 6 {
                        if body[1] & 0x0F != 0 {
                            track_times.push(ms);
                        }
                    } else if body[1] == 1 {
                        main_times.push(ms);
                        let cts = ((body[2] as i32) << 16 | (body[3] as i32) << 8 | (body[4] as i32)) << 8 >> 8;
                        cts_list.push(cts);
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        let end_tags = pipe.finish().unwrap_or_else(|e| panic!("{e}"));
        for t in end_tags {
            if let Tag::Video { ms, body } = t {
                if body[0] & 0x80 != 0 && body[0] & 0x0F == 6 {
                    if body[1] & 0x0F != 0 {
                        track_times.push(ms);
                    }
                } else if body[1] == 1 {
                    main_times.push(ms);
                    let cts = ((body[2] as i32) << 16 | (body[3] as i32) << 8 | (body[4] as i32)) << 8 >> 8;
                    cts_list.push(cts);
                }
            }
        }
        assert!(main_times.len() >= 30, "got {}", main_times.len());
        assert!(
            main_times.windows(2).all(|w| w[1] > w[0]),
            "main stream dts must strictly increase from frame 1: {:?}",
            &main_times[..10.min(main_times.len())]
        );
        assert!(cts_list.iter().all(|c| *c >= 0), "cts must never be negative: {cts_list:?}");
        if !track_times.is_empty() {
            assert!(
                track_times.windows(2).all(|w| w[1] > w[0]),
                "extra track dts must strictly increase: {:?}",
                &track_times[..10.min(track_times.len())]
            );
        }
    }

    #[test]
    fn reshape_and_heal_never_move_dts_backwards() {
        // Test 1: set_video toggling bframes false -> true (and 60fps -> 30fps with bframes=true).
        let cfg60 = VideoConfig { fps_num: 60, ..video(true) };
        let Ok(mut pipe) =
            Pipeline::new(cfg60, AudioConfig { sample_rate: 48000, channels: 2, bitrate_kbps: 128 }, None)
        else {
            return;
        };
        let mut all_ms = Vec::new();
        for i in 0..30u64 {
            let ts = i * 16_666_667;
            let y: Vec<u8> = (0..320 * 180).map(|p| ((p + i as usize * 7) % 251) as u8).collect();
            let uv = vec![128u8; 320 * 90];
            for t in pipe
                .video(Frame::Nv12 { y: &y, y_stride: 320, uv: &uv, uv_stride: 320 }, ts)
                .unwrap_or_else(|e| panic!("{e}"))
            {
                if let Tag::Video { ms, .. } = t {
                    all_ms.push(ms);
                }
            }
        }
        // Reshape to 30fps bframes=true (dts_shift jumps from 50ms to 100ms).
        let next = VideoEncoder::new(video(true)).unwrap_or_else(|e| panic!("{e}"));
        for t in pipe.set_video(next).unwrap_or_else(|e| panic!("{e}")) {
            if let Tag::Video { ms, .. } = t {
                all_ms.push(ms);
            }
        }
        for i in 15..30u64 {
            let ts = i * 33_333_333;
            let y: Vec<u8> = (0..320 * 180).map(|p| ((p + i as usize * 7) % 251) as u8).collect();
            let uv = vec![128u8; 320 * 90];
            for t in pipe
                .video(Frame::Nv12 { y: &y, y_stride: 320, uv: &uv, uv_stride: 320 }, ts)
                .unwrap_or_else(|e| panic!("{e}"))
            {
                if let Tag::Video { ms, .. } = t {
                    all_ms.push(ms);
                }
            }
        }
        // Also test heal() mid-stream:
        pipe.heal().unwrap_or_else(|e| panic!("{e}"));
        for i in 30..45u64 {
            let ts = i * 33_333_333;
            let y: Vec<u8> = (0..320 * 180).map(|p| ((p + i as usize * 7) % 251) as u8).collect();
            let uv = vec![128u8; 320 * 90];
            for t in pipe
                .video(Frame::Nv12 { y: &y, y_stride: 320, uv: &uv, uv_stride: 320 }, ts)
                .unwrap_or_else(|e| panic!("{e}"))
            {
                if let Tag::Video { ms, .. } = t {
                    all_ms.push(ms);
                }
            }
        }
        for t in pipe.finish().unwrap_or_else(|e| panic!("{e}")) {
            if let Tag::Video { ms, .. } = t {
                all_ms.push(ms);
            }
        }
        assert!(all_ms.windows(2).all(|w| w[1] >= w[0]), "dts jumped backwards across reshape/heal: {all_ms:?}");
    }

    #[test]
    fn real_vt_bframes_recording_pause_resume_decodes_cleanly_in_ffmpeg() {
        if !have("ffmpeg") || !have("ffprobe") {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ferrocore-bframe-pause-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("paused.mp4");
        let Ok(mut pipe) =
            Pipeline::new(video(true), AudioConfig { sample_rate: 48000, channels: 2, bitrate_kbps: 128 }, Some(&f))
        else {
            return;
        };
        let pcm = vec![0.05f32; 1600 * 2];
        for i in 0..180u64 {
            if i == 60 {
                pipe.pause_recording(true);
            }
            if i == 110 {
                pipe.pause_recording(false);
            }
            let ts = i * 33_333_333;
            let y: Vec<u8> = (0..320 * 180).map(|p| ((p + i as usize * 7) % 251) as u8).collect();
            let uv = vec![128u8; 320 * 90];
            pipe.video(Frame::Nv12 { y: &y, y_stride: 320, uv: &uv, uv_stride: 320 }, ts)
                .unwrap_or_else(|e| panic!("{e}"));
            pipe.audio(&pcm, ts).unwrap_or_else(|e| panic!("{e}"));
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        pipe.finish().unwrap_or_else(|e| panic!("{e}"));
        let dec = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&f)
            .args(["-f", "null", "-"])
            .output()
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(
            dec.stderr.is_empty(),
            "ffmpeg decode errors after pause/resume: {}",
            String::from_utf8_lossy(&dec.stderr)
        );
        let v = packets(&f, "v:0");
        assert!(v.len() >= 100 && v.len() <= 140, "paused segment cut cleanly: {} frames", v.len());
        assert!(v.windows(2).all(|w| w[1].1 > w[0].1), "mp4 decode times strictly rise across pause/resume");
        assert!(v.iter().all(|p| p.1 <= p.0 + 1e-6), "never displayed before decoded");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 60 fps 2+ GOP pipeline with B-frames (including simulated engine stalls): FLV tag DTS
    /// never hits 1 ms clamp steps and every packet's display time matches its true capture time.
    #[test]
    fn bframes_60fps_pipeline_flv_tags_have_steady_dts_and_exact_pts() {
        let cfg = VideoConfig {
            fps_num: 60,
            fps_den: 1,
            keyframe_secs: 1,
            width: 640,
            height: 360,
            bitrate_kbps: 3000,
            ..video(true)
        };
        let Ok(mut pipe) = Pipeline::new(cfg, AudioConfig { sample_rate: 48000, channels: 2, bitrate_kbps: 128 }, None)
        else {
            return;
        };
        let dts_shift_us = 50_000i64;
        let frame_ns = 16_666_667u64;
        let y_base = vec![90u8; 640 * 360];
        let uv = vec![128u8; 640 * 180];
        let mut tags = Vec::new();
        let mut input_pts_ms = Vec::new();
        let mut ts = 0u64;
        for i in 0..160u64 {
            if matches!(i, 14 | 31 | 63 | 80) {
                ts += frame_ns * 3;
            } else if i > 0 {
                ts += frame_ns;
            }
            input_pts_ms.push(((ts / 1000) as i64 + dts_shift_us) / 1000);
            let mut y = y_base.clone();
            let stripe = ((i as usize * 37) % 360) * 640;
            let end = (stripe + 640 * 6).min(y.len());
            for b in &mut y[stripe..end] {
                *b = ((i * 23) % 251) as u8;
            }
            tags.extend(
                pipe.video(Frame::Nv12 { y: &y, y_stride: 640, uv: &uv, uv_stride: 640 }, ts)
                    .unwrap_or_else(|e| panic!("{e}")),
            );
        }
        tags.extend(pipe.finish().unwrap_or_else(|e| panic!("{e}")));

        let mut dts_ms = Vec::new();
        let mut pts_ms = Vec::new();
        let mut keys = 0usize;
        for t in tags {
            if let Tag::Video { ms, body } = t
                && body.len() > 5
                && body[1] == 1
            {
                if body[0] >> 4 == 1 {
                    keys += 1;
                }
                let cts = ((body[2] as i32) << 16 | (body[3] as i32) << 8 | (body[4] as i32)) << 8 >> 8;
                assert!(cts >= 0, "negative cts {cts}");
                dts_ms.push(i64::from(ms));
                pts_ms.push(i64::from(ms) + i64::from(cts));
            }
        }
        assert_eq!(dts_ms.len(), 160);
        assert!(keys >= 2, "expected 2+ GOPs, got {keys} keyframes");
        assert!(dts_ms.windows(2).all(|w| w[1] - w[0] >= 8), "no 1 ms DTS clamp steps: {dts_ms:?}");
        pts_ms.sort_unstable();
        for (got, want) in pts_ms.iter().zip(&input_pts_ms) {
            assert!((got - want).abs() <= 1, "display time altered: got={got} ms want={want} ms");
        }
    }
}
