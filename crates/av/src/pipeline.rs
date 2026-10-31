//! One output: frames and sound in, on one clock (nanoseconds, libobs's or ours), encoded once,
//! then sent as FLV tags (to Go live's hub, which streams to every platform) and recorded to a
//! crash-safe MP4, both from the same packets. Time 0 is the first frame; sound from before it
//! is left out, so picture and sound start together.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use crate::AvError;
use crate::audio::{AudioConfig, AudioEncoder, AudioPacket};
use crate::flv;
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
}

#[derive(Debug)]
enum Recording {
    /// Waiting for the encoder's codec header (after the first frame).
    Waiting {
        file: BufWriter<File>,
        held: Vec<AudioPacket>,
    },
    On(Box<Mp4Writer<BufWriter<File>>>),
    Failed,
}

impl Pipeline {
    /// Starts the encoders; with `record`, also writes an MP4 there (made now, so a path that
    /// can't be written fails at once).
    pub fn new(video: VideoConfig, audio: AudioConfig, record: Option<&Path>) -> Result<Self, AvError> {
        let recording = match record {
            Some(p) => {
                let f = File::create(p).map_err(|e| {
                    AvError::Encoder(format!("the recording can't be written at {} ({e})", p.display()))
                })?;
                Some(Recording::Waiting { file: BufWriter::new(f), held: Vec::new() })
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
        })
    }

    /// A frame at `ts_ns`. The first one is a keyframe and starts the clock.
    pub fn video(&mut self, frame: Frame<'_>, ts_ns: u64) -> Result<Vec<Tag>, AvError> {
        let start = *self.start_ns.get_or_insert(ts_ns);
        let first = !self.sent_video_header && self.video.header().is_none();
        let pts = (ts_ns.saturating_sub(start) / 1000) as i64;
        let packets = self.video.encode(frame, pts, first)?;
        self.video_out(packets)
    }

    /// Sound (interleaved, the configured channels) whose first sample is at `ts_ns`.
    pub fn audio(&mut self, pcm: &[f32], ts_ns: u64) -> Result<Vec<Tag>, AvError> {
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
            self.record_audio(p);
        }
        Ok(tags)
    }

    /// Ends: what's still in the encoder is sent, and the recording closed.
    pub fn finish(mut self) -> Result<Vec<Tag>, AvError> {
        let packets = self.video.flush()?;
        let mut tags = self.video_out(packets)?;
        let offset = self.audio_offset_us.unwrap_or(0);
        for mut p in self.audio.flush()? {
            p.pts_us += offset;
            tags.push(Tag::Audio { ms: (p.pts_us / 1000) as u32, body: flv::audio_tag(&p.data) });
            self.record_audio(p);
        }
        if let Some(Recording::On(w)) = self.recording.take() {
            w.finish().map_err(|e| AvError::Encoder(format!("the recording couldn't be finished ({e})")))?;
        }
        Ok(tags)
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
        let header = self.video.header();
        let cfg = *self.video.config();
        let acfg = *self.audio.config();
        let aheader = self.audio.header();
        let rec = match self.recording.take() {
            Some(Recording::Waiting { file, held }) => match header {
                Some(config) => {
                    let v = VideoTrack { codec: cfg.codec, width: cfg.width, height: cfg.height, config };
                    let a = AudioTrack {
                        sample_rate: acfg.sample_rate,
                        channels: acfg.channels,
                        config: aheader,
                        bitrate_kbps: acfg.bitrate_kbps,
                    };
                    match Mp4Writer::new(file, Some(v), Some(a)) {
                        Ok(mut w) => {
                            let ok = w.video(p).is_ok() && held.iter().all(|a| w.audio(a).is_ok());
                            if ok { Recording::On(Box::new(w)) } else { Recording::Failed }
                        }
                        Err(_) => Recording::Failed,
                    }
                }
                None => Recording::Waiting { file, held },
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

    fn record_audio(&mut self, p: AudioPacket) {
        match &mut self.recording {
            Some(Recording::Waiting { held, .. }) => held.push(p),
            Some(Recording::On(w)) => {
                if w.audio(&p).is_err() {
                    self.recording = Some(Recording::Failed);
                }
            }
            _ => {}
        }
    }
}
