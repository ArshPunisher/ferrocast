//! Audio encoding: interleaved 32-bit float sound in, AAC-LC frames out (what RTMP platforms and
//! MP4 take). macOS uses AudioToolbox's encoder (`apple/aac.rs`).

use crate::AvError;

/// Samples per AAC frame, per channel.
pub const AAC_FRAME: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioConfig {
    /// 48000 or 44100.
    pub sample_rate: u32,
    /// 1 or 2.
    pub channels: u32,
    pub bitrate_kbps: u32,
}

/// One AAC frame (raw, no ADTS header). Time in microseconds from the first sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket {
    pub pts_us: i64,
    pub data: Vec<u8>,
}

/// The AudioSpecificConfig for AAC-LC (FLV's and MP4's audio header): 5 bits object type,
/// 4 bits rate index, 4 bits channels, 3 bits zero.
pub fn aac_config(sample_rate: u32, channels: u32) -> Option<[u8; 2]> {
    const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];
    let i = RATES.iter().position(|r| *r == sample_rate)? as u16;
    if !(1..=7).contains(&channels) {
        return None;
    }
    let v: u16 = (2 << 11) | (i << 7) | ((channels as u16) << 3);
    Some(v.to_be_bytes())
}

enum Inner {
    #[cfg(target_os = "macos")]
    Native(crate::apple::aac::Encoder),
    Ffmpeg(crate::ffenc::Audio),
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(target_os = "macos")]
            Self::Native(_) => f.write_str("AudioToolbox"),
            Self::Ffmpeg(a) => a.fmt(f),
        }
    }
}

/// The encoding itself: samples in, AAC frames out, timed by the samples counted.
#[derive(Debug)]
struct Core {
    inner: Inner,
    config: AudioConfig,
    pending: Vec<f32>,
    /// AAC frames made so far: the next one's time.
    frames: i64,
}

impl Core {
    fn flush(&mut self) -> Vec<AudioPacket> {
        let rest = match &mut self.inner {
            #[cfg(target_os = "macos")]
            Inner::Native(_) => Vec::new(),
            Inner::Ffmpeg(e) => e.flush(),
        };
        let priming = self.priming();
        self.stamp(rest, priming)
    }

    fn priming(&self) -> u32 {
        match &self.inner {
            #[cfg(target_os = "macos")]
            Inner::Native(e) => e.priming,
            Inner::Ffmpeg(_) => crate::ffenc::PRIMING,
        }
    }

    /// Times for frames as they come out: the encoder's output starts `priming` samples early
    /// (its warm-up); frames wholly inside it are dropped; times are where the real sound is.
    fn stamp(&mut self, made: Vec<Vec<u8>>, priming: u32) -> Vec<AudioPacket> {
        let mut out = Vec::new();
        for data in made {
            let at = self.frames * AAC_FRAME as i64 - i64::from(priming);
            self.frames += 1;
            if at + (AAC_FRAME as i64) <= 0 {
                continue;
            }
            let pts_us = at.max(0) * 1_000_000 / i64::from(self.config.sample_rate);
            out.push(AudioPacket { pts_us, data });
        }
        out
    }

    fn encode(&mut self, pcm: &[f32]) -> Result<Vec<AudioPacket>, AvError> {
        self.pending.extend_from_slice(pcm);
        let n = AAC_FRAME * self.config.channels as usize;
        let mut out = Vec::new();
        while self.pending.len() >= n {
            let made = match &mut self.inner {
                #[cfg(target_os = "macos")]
                Inner::Native(e) => e.encode(&self.pending[..n])?,
                Inner::Ffmpeg(e) => e.encode(&self.pending[..n])?,
            };
            let priming = self.priming();
            out.extend(self.stamp(made, priming));
            self.pending.drain(..n);
        }
        Ok(out)
    }
}

/// What the encoder's own thread is asked.
enum Job {
    Pcm(Vec<f32>),
    /// Answered with `Reply::Fence` once every sample before it is encoded.
    Fence,
    Flush,
}

enum Reply {
    Packets(Result<Vec<AudioPacket>, AvError>),
    Fence,
    Flushed(Vec<AudioPacket>),
}

/// The encoder on a thread of its own: samples queue to it, packets come back to be taken
/// without waiting.
#[derive(Debug)]
struct Threaded {
    tx: Option<std::sync::mpsc::SyncSender<Job>>,
    rx: std::sync::mpsc::Receiver<Reply>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// Samples the queue had no room for: sent with the next call, none are ever dropped (the
    /// packets' times count samples, so a gap would move all later sound).
    backlog: Vec<f32>,
}

/// Jobs queued at most (about 2.5 s of 10 ms blocks); beyond that samples wait in the backlog.
const QUEUE: usize = 256;
/// The backlog's limit, in seconds of sound: past it the encoder is called stuck.
const BACKLOG_SECS: usize = 10;

impl Threaded {
    fn spawn(mut core: Core) -> Result<Self, AvError> {
        let (tx, jobs) = std::sync::mpsc::sync_channel::<Job>(QUEUE);
        let (out, rx) = std::sync::mpsc::channel::<Reply>();
        let handle = std::thread::Builder::new()
            .name("ferrocast-aac".into())
            .spawn(move || {
                while let Ok(job) = jobs.recv() {
                    let reply = match job {
                        Job::Pcm(pcm) => Reply::Packets(core.encode(&pcm)),
                        Job::Fence => Reply::Fence,
                        Job::Flush => Reply::Flushed(core.flush()),
                    };
                    if out.send(reply).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| AvError::Encoder(format!("the sound encoder's thread didn't start ({e})")))?;
        Ok(Self { tx: Some(tx), rx, handle: Some(handle), backlog: Vec::new() })
    }

    fn gone() -> AvError {
        AvError::Encoder("the sound encoder stopped".into())
    }

    /// Queues `pcm` (with any backlog) without waiting.
    fn push(&mut self, pcm: &[f32], limit: usize) -> Result<(), AvError> {
        let tx = self.tx.as_ref().ok_or_else(Self::gone)?;
        let mut job = std::mem::take(&mut self.backlog);
        job.extend_from_slice(pcm);
        if job.is_empty() {
            return Ok(());
        }
        match tx.try_send(Job::Pcm(job)) {
            Ok(()) => Ok(()),
            Err(std::sync::mpsc::TrySendError::Full(Job::Pcm(back))) => {
                if back.len() > limit {
                    return Err(AvError::Encoder("the sound encoder fell too far behind".into()));
                }
                self.backlog = back;
                Ok(())
            }
            Err(_) => Err(Self::gone()),
        }
    }

    /// Packets finished so far.
    fn take(&mut self) -> Result<Vec<AudioPacket>, AvError> {
        let mut out = Vec::new();
        while let Ok(r) = self.rx.try_recv() {
            match r {
                Reply::Packets(p) => out.extend(p?),
                Reply::Fence | Reply::Flushed(_) => {}
            }
        }
        Ok(out)
    }

    /// Waits until everything pushed is encoded; its packets.
    fn fence(&mut self, flush: bool) -> Result<Vec<AudioPacket>, AvError> {
        let tx = self.tx.as_ref().ok_or_else(Self::gone)?;
        if !self.backlog.is_empty() {
            tx.send(Job::Pcm(std::mem::take(&mut self.backlog))).map_err(|_| Self::gone())?;
        }
        tx.send(if flush { Job::Flush } else { Job::Fence }).map_err(|_| Self::gone())?;
        let mut out = Vec::new();
        loop {
            match self.rx.recv().map_err(|_| Self::gone())? {
                Reply::Packets(p) => out.extend(p?),
                Reply::Fence if !flush => return Ok(out),
                Reply::Flushed(p) if flush => {
                    out.extend(p);
                    return Ok(out);
                }
                Reply::Fence | Reply::Flushed(_) => {}
            }
        }
    }
}

impl Drop for Threaded {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[derive(Debug)]
enum Mode {
    Here(Core),
    Thread(Threaded),
}

/// An AAC encoder: the system's own (AudioToolbox on a Mac), else FFmpeg's. It encodes in the
/// caller's thread, or (`off_thread`) on one of its own: `encode` then only queues the sound
/// and hands back what the thread has finished, the same packets with the same times a little
/// later; `settle` waits for all of it.
#[derive(Debug)]
pub struct AudioEncoder {
    mode: Mode,
    config: AudioConfig,
}

impl AudioEncoder {
    pub fn new(config: AudioConfig) -> Result<Self, AvError> {
        Self::check(&config)?;
        #[cfg(target_os = "macos")]
        if !std::env::var("FERROCAST_AV_ENCODER").is_ok_and(|v| v == "ffmpeg") {
            let inner = Inner::Native(crate::apple::aac::Encoder::new(&config)?);
            return Ok(Self { mode: Mode::Here(Core { inner, config, pending: Vec::new(), frames: 0 }), config });
        }
        Self::ffmpeg(config)
    }

    /// Through FFmpeg, whatever the system.
    pub fn ffmpeg(config: AudioConfig) -> Result<Self, AvError> {
        Self::check(&config)?;
        let inner = Inner::Ffmpeg(crate::ffenc::Audio::new(&config)?);
        Ok(Self { mode: Mode::Here(Core { inner, config, pending: Vec::new(), frames: 0 }), config })
    }

    /// Moves the encoding to a thread of its own (nothing encoded yet is lost: call it before
    /// the first sample). Already off the thread: unchanged.
    pub fn off_thread(self) -> Result<Self, AvError> {
        let Self { mode, config } = self;
        match mode {
            Mode::Here(core) => Ok(Self { mode: Mode::Thread(Threaded::spawn(core)?), config }),
            thread => Ok(Self { mode: thread, config }),
        }
    }

    /// Whether it encodes on its own thread.
    pub fn is_off_thread(&self) -> bool {
        matches!(self.mode, Mode::Thread(_))
    }

    fn check(config: &AudioConfig) -> Result<(), AvError> {
        if aac_config(config.sample_rate, config.channels).is_none() || config.channels > 2 {
            return Err(AvError::Encoder(format!(
                "AAC takes 1 or 2 channels at a standard rate, not {} at {} Hz",
                config.channels, config.sample_rate
            )));
        }
        Ok(())
    }

    pub fn config(&self) -> &AudioConfig {
        &self.config
    }

    /// The AudioSpecificConfig.
    pub fn header(&self) -> Vec<u8> {
        aac_config(self.config.sample_rate, self.config.channels).map(|c| c.to_vec()).unwrap_or_default()
    }

    /// Ends: the frames still inside the encoder (FFmpeg keeps some back; the Mac's gives each
    /// at once), and on a thread, everything queued first. Samples short of a whole frame are
    /// left out.
    pub fn flush(&mut self) -> Result<Vec<AudioPacket>, AvError> {
        match &mut self.mode {
            Mode::Here(c) => Ok(c.flush()),
            Mode::Thread(t) => t.fence(true),
        }
    }

    /// Adds interleaved samples; returns every whole AAC frame now ready (on a thread: those it
    /// has finished, without waiting).
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<AudioPacket>, AvError> {
        let limit = BACKLOG_SECS * self.config.sample_rate as usize * self.config.channels as usize;
        match &mut self.mode {
            Mode::Here(c) => c.encode(pcm),
            Mode::Thread(t) => {
                t.push(pcm, limit)?;
                t.take()
            }
        }
    }

    /// Waits until all sound given so far is encoded, and returns what that made (nothing when
    /// it encodes here: `encode` returned it already). For the moments the output's files or
    /// pauses change, so packets land on the same side of them as when encoding here.
    pub fn settle(&mut self) -> Result<Vec<AudioPacket>, AvError> {
        match &mut self.mode {
            Mode::Here(_) => Ok(Vec::new()),
            Mode::Thread(t) => t.fence(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tone in blocks of 480 frames (stereo), as the mixer gives it.
    fn tone(blocks: usize) -> Vec<Vec<f32>> {
        (0..blocks)
            .map(|b| {
                (0..480)
                    .flat_map(|n| {
                        let v = (((b * 480 + n) as f32) * 0.03).sin() * 0.4;
                        [v, -v]
                    })
                    .collect()
            })
            .collect()
    }

    fn cfg() -> AudioConfig {
        AudioConfig { sample_rate: 48000, channels: 2, bitrate_kbps: 128 }
    }

    /// On a thread of its own the encoder makes the same frames with the same times: what comes
    /// back is the same list, at the end (and `settle` hands over what is owed meanwhile).
    #[test]
    fn a_thread_makes_the_same_packets_with_the_same_times() {
        let blocks = tone(400);
        let mut here = AudioEncoder::new(cfg()).unwrap_or_else(|e| panic!("{e}"));
        let mut there = AudioEncoder::new(cfg()).and_then(AudioEncoder::off_thread).unwrap_or_else(|e| panic!("{e}"));
        assert!(there.is_off_thread() && !here.is_off_thread());
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for (i, blk) in blocks.iter().enumerate() {
            a.extend(here.encode(blk).unwrap_or_else(|e| panic!("{e}")));
            b.extend(there.encode(blk).unwrap_or_else(|e| panic!("{e}")));
            if i % 97 == 50 {
                // A cut: everything given so far is in `b` now, as in `a`.
                b.extend(there.settle().unwrap_or_else(|e| panic!("{e}")));
                assert_eq!(a.len(), b.len(), "settled at block {i}");
            }
        }
        a.extend(here.flush().unwrap_or_else(|e| panic!("{e}")));
        b.extend(there.flush().unwrap_or_else(|e| panic!("{e}")));
        assert!(a.len() > 150, "{} packets", a.len());
        assert_eq!(a, b, "same packets, same times");
    }

    /// Prints what `encode` costs the calling thread, here and with the thread (`--nocapture`).
    #[test]
    fn the_calling_thread_pays_only_for_queueing() {
        let blocks = tone(1000);
        let time = |mut e: AudioEncoder| {
            let mut worst = std::time::Duration::ZERO;
            let t0 = std::time::Instant::now();
            for b in &blocks {
                let t = std::time::Instant::now();
                let _ = e.encode(b);
                worst = worst.max(t.elapsed());
            }
            let total = t0.elapsed();
            let _ = e.flush();
            (total, worst)
        };
        let here = time(AudioEncoder::new(cfg()).unwrap_or_else(|e| panic!("{e}")));
        let there = time(AudioEncoder::new(cfg()).and_then(AudioEncoder::off_thread).unwrap_or_else(|e| panic!("{e}")));
        println!(
            "encode() on the caller: here {:.1} us/block (worst {:.0} us), on a thread {:.1} us/block (worst {:.0} us)",
            here.0.as_secs_f64() * 1e6 / 1000.0,
            here.1.as_secs_f64() * 1e6,
            there.0.as_secs_f64() * 1e6 / 1000.0,
            there.1.as_secs_f64() * 1e6
        );
        assert!(there.0 < here.0, "queueing is cheaper than encoding");
    }

    /// A queue with no room keeps the sound (nothing is dropped, so no later time moves).
    #[test]
    fn a_full_queue_keeps_the_sound() {
        let mut here = AudioEncoder::new(cfg()).unwrap_or_else(|e| panic!("{e}"));
        let mut there = AudioEncoder::new(cfg()).and_then(AudioEncoder::off_thread).unwrap_or_else(|e| panic!("{e}"));
        // Far more blocks than the queue holds, given without ever taking time to let it drain.
        let blocks = tone(QUEUE * 3);
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for blk in &blocks {
            a.extend(here.encode(blk).unwrap_or_else(|e| panic!("{e}")));
            b.extend(there.encode(blk).unwrap_or_else(|e| panic!("{e}")));
        }
        a.extend(here.flush().unwrap_or_else(|e| panic!("{e}")));
        b.extend(there.flush().unwrap_or_else(|e| panic!("{e}")));
        assert_eq!(a, b);
    }

    #[test]
    fn config_for_48k_stereo() {
        assert_eq!(aac_config(48000, 2), Some([0x11, 0x90]));
        assert_eq!(aac_config(44100, 2), Some([0x12, 0x10]));
        assert_eq!(aac_config(44101, 2), None);
    }
}
