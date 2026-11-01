//! Sound in through cpal (where there's no capture API of ours: Windows): a microphone, or
//! what the computer plays (WASAPI's loopback: an output device opened as an input). Whatever
//! the device's format, the engine gets 48 kHz stereo floats, stamped with its clock.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::AvError;
use crate::engine::now_ns;

/// Sound kept for the engine: 48 kHz stereo pieces with when they came.
type Pieces = Arc<Mutex<VecDeque<(Vec<f32>, u64)>>>;

/// Sound kept before it's taken (about 5 s).
const MAX_PIECES: usize = 500;
const RATE: u32 = 48_000;

/// Which sound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Which {
    /// A microphone by name (None, "" or "default": the system's).
    Input(Option<String>),
    /// What the computer plays (Windows: the default speakers, as WASAPI loopback).
    Loopback,
}

/// Microphones and other inputs connected now, by name.
pub fn inputs() -> Vec<String> {
    cpal::default_host()
        .input_devices()
        .map(|ds| ds.filter_map(|d| d.description().ok().map(|n| n.to_string())).collect())
        .unwrap_or_default()
}

/// Turns the device's frames into 48 kHz stereo, in pieces (linear between samples: enough
/// for a voice or desktop sound).
#[derive(Debug)]
struct Convert {
    channels: usize,
    step: f64,
    /// Where the next output frame falls, in input frames from `last`.
    pos: f64,
    last: [f32; 2],
    out: Vec<f32>,
}

impl Convert {
    fn new(channels: usize, rate: u32) -> Self {
        Convert {
            channels: channels.max(1),
            step: f64::from(rate) / f64::from(RATE),
            pos: 0.0,
            last: [0.0; 2],
            out: Vec::new(),
        }
    }

    /// Interleaved input → finished stereo pieces of 1024 frames.
    fn push(&mut self, data: &[f32]) -> Vec<Vec<f32>> {
        let frames = data.len() / self.channels;
        let frame = |i: usize| -> [f32; 2] {
            let f = &data[i * self.channels..(i + 1) * self.channels];
            if f.len() >= 2 { [f[0], f[1]] } else { [f[0], f[0]] }
        };
        // Output frames between `last` (input frame -1) and each input frame.
        for i in 0..frames {
            let next = frame(i);
            while self.pos < 1.0 {
                let t = self.pos as f32;
                self.out.push(self.last[0] + (next[0] - self.last[0]) * t);
                self.out.push(self.last[1] + (next[1] - self.last[1]) * t);
                self.pos += self.step;
            }
            self.pos -= 1.0;
            self.last = next;
        }
        let mut pieces = Vec::new();
        while self.out.len() >= 2048 {
            pieces.push(self.out.drain(..2048).collect());
        }
        pieces
    }
}

/// Sound coming in; dropping it stops.
#[derive(Debug)]
pub struct SoundIn {
    sound: Pieces,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for SoundIn {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl SoundIn {
    /// Starts `which` (on its own thread: a stream stays where it was made).
    pub fn start(which: Which) -> Result<Self, AvError> {
        let sound: Pieces = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let (s, st) = (sound.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("ferrocast-sound-in".into())
            .spawn(move || {
                let stream = match open(&which, s) {
                    Ok(stream) => stream,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                if let Err(e) = stream.play() {
                    let _ = ready_tx.send(Err(format!("it didn't start ({e})")));
                    return;
                }
                let _ = ready_tx.send(Ok(()));
                while !st.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(50));
                }
                drop(stream);
            })
            .map_err(|e| AvError::Source(e.to_string()))?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(SoundIn { sound, stop, thread: Some(thread) }),
            Ok(Err(e)) => Err(AvError::Source(e)),
            Err(_) => {
                // Late: if it opens after all, it closes again at once.
                stop.store(true, Ordering::Release);
                Err(AvError::Source("the sound device didn't answer".into()))
            }
        }
    }

    /// Sound since the last call, oldest first: 48 kHz stereo.
    pub fn take_sound(&self) -> Vec<(Vec<f32>, u64)> {
        self.sound.lock().map(|mut q| q.drain(..).collect()).unwrap_or_default()
    }
}

/// The device and its stream, feeding `sound`.
fn open(which: &Which, sound: Pieces) -> Result<cpal::Stream, String> {
    let host = cpal::default_host();
    let dev = match which {
        Which::Loopback => host.default_output_device().ok_or("there are no speakers to hear the computer from")?,
        Which::Input(None) => host.default_input_device().ok_or("there's no microphone")?,
        Which::Input(Some(n)) if n.is_empty() || n == "default" => {
            host.default_input_device().ok_or("there's no microphone")?
        }
        Which::Input(Some(name)) => host
            .input_devices()
            .ok()
            .and_then(|mut ds| ds.find(|d| d.description().is_ok_and(|x| x.to_string() == *name)))
            .ok_or_else(|| format!("the microphone \"{name}\" isn't connected"))?,
    };
    let cfg = match which {
        Which::Loopback => dev.default_output_config(),
        Which::Input(_) => dev.default_input_config(),
    }
    .map_err(|e| format!("its format couldn't be read ({e})"))?;
    let mut conv = Convert::new(usize::from(cfg.channels()), cfg.sample_rate());
    let mut keep = move |data: &[f32]| {
        let now = now_ns();
        let pieces = conv.push(data);
        if pieces.is_empty() {
            return;
        }
        if let Ok(mut q) = sound.lock() {
            for p in pieces {
                q.push_back((p, now));
            }
            while q.len() > MAX_PIECES {
                q.pop_front();
            }
        }
    };
    let err = |e: cpal::Error| tracing::warn!(%e, "a sound input stopped");
    let config = cfg.config();
    match cfg.sample_format() {
        cpal::SampleFormat::F32 => dev.build_input_stream(config, move |d: &[f32], _: &_| keep(d), err, None),
        cpal::SampleFormat::I16 => dev.build_input_stream(
            config,
            move |d: &[i16], _: &_| keep(&d.iter().map(|v| f32::from(*v) / 32768.0).collect::<Vec<_>>()),
            err,
            None,
        ),
        cpal::SampleFormat::I32 => dev.build_input_stream(
            config,
            move |d: &[i32], _: &_| keep(&d.iter().map(|v| *v as f32 / 2_147_483_648.0).collect::<Vec<_>>()),
            err,
            None,
        ),
        other => return Err(format!("its sample format ({other}) isn't supported")),
    }
    .map_err(|e| format!("it couldn't be opened ({e})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_rate_and_layout_becomes_48k_stereo() {
        // 44.1 kHz mono: one second → 48000 frames, both sides the same.
        let mut c = Convert::new(1, 44_100);
        let input: Vec<f32> = (0..44_100).map(|i| (i as f32 * 0.01).sin()).collect();
        let pieces = c.push(&input);
        let frames: usize = pieces.iter().map(|p| p.len() / 2).sum::<usize>() + c.out.len() / 2;
        assert!((47_990..=48_010).contains(&frames), "{frames}");
        assert!(pieces.iter().all(|p| p.len() == 2048 && p.as_chunks::<2>().0.iter().all(|f| f[0] == f[1])));
        // 48 kHz with six channels: the first two are kept, nothing is stretched.
        let mut c = Convert::new(6, 48_000);
        let six: Vec<f32> = (0..2048 * 6).map(|i| if i % 6 == 1 { 0.5 } else { 0.25 }).collect();
        let p = c.push(&six);
        assert_eq!(p.len(), 2, "2048 frames: two pieces of 1024");
        assert_eq!(&p[0][2..4], &[0.25, 0.5]);
    }
}
