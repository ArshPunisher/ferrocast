//! Hearing sound here (OBS's Audio Monitoring): the mixer's monitor mix played on an output
//! device (the system's, or one by name), through cpal. The engine pushes each block; the
//! device's callback takes what's there and plays silence when it runs short (counted).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::AvError;

/// At most this much waits to be played (a stalled device can't grow memory, and monitoring
/// shouldn't lag): 200 ms.
const MAX_MS: usize = 200;

/// Output devices here, by name.
pub fn devices() -> Vec<String> {
    let host = cpal::default_host();
    host.output_devices()
        .map(|d| d.filter_map(|d| d.description().ok().map(|n| n.to_string())).collect())
        .unwrap_or_default()
}

struct Ring {
    samples: Mutex<VecDeque<f32>>,
    short: AtomicU64,
    played: AtomicU64,
}

/// Monitoring on a device; dropping it stops. Its [`Feeder`] can go to another thread.
pub struct Monitor {
    _stream: cpal::Stream,
    feeder: Feeder,
}

impl std::fmt::Debug for Monitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Monitor").field("feeder", &self.feeder).finish_non_exhaustive()
    }
}

/// What sound is pushed into: shared with the device's callback.
#[derive(Clone)]
pub struct Feeder {
    ring: Arc<Ring>,
    state: Arc<Mutex<Resample>>,
}

impl std::fmt::Debug for Feeder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Feeder").finish_non_exhaustive()
    }
}

/// The device's rate and channels, and where the resampler is between the mixer's frames.
struct Resample {
    rate: u32,
    channels: usize,
    src_rate: u32,
    phase: f64,
    last: [f32; 2],
}

impl Monitor {
    /// Starts on `device` (a name from [`devices`]; None is the system's), for sound at
    /// `src_rate` (the mixer's).
    pub fn start(device: Option<&str>, src_rate: u32) -> Result<Self, AvError> {
        let host = cpal::default_host();
        let dev = match device {
            None => host.default_output_device(),
            Some(name) => host
                .output_devices()
                .ok()
                .and_then(|mut ds| ds.find(|d| d.description().is_ok_and(|n| n.to_string() == name))),
        }
        .ok_or_else(|| {
            AvError::Source(format!("the speakers \"{}\" aren't connected", device.unwrap_or("the system's")))
        })?;
        let cfg = dev.default_output_config().map_err(|e| AvError::Source(format!("the speakers can't play ({e})")))?;
        let (rate, channels) = (cfg.sample_rate(), usize::from(cfg.channels()));
        let ring = Arc::new(Ring {
            samples: Mutex::new(VecDeque::new()),
            short: AtomicU64::new(0),
            played: AtomicU64::new(0),
        });
        let r = ring.clone();
        let stream = dev
            .build_output_stream(
                cfg.config(),
                move |out: &mut [f32], _| {
                    let Ok(mut q) = r.samples.lock() else { return };
                    let mut short = false;
                    for v in out.iter_mut() {
                        *v = match q.pop_front() {
                            Some(s) => s,
                            None => {
                                short = true;
                                0.0
                            }
                        };
                    }
                    r.played.fetch_add(out.len() as u64, Ordering::Relaxed);
                    if short {
                        r.short.fetch_add(1, Ordering::Relaxed);
                    }
                },
                |e| tracing::warn!(%e, "monitoring stopped"),
                None,
            )
            .map_err(|e| AvError::Source(format!("the speakers can't play ({e})")))?;
        stream.play().map_err(|e| AvError::Source(format!("the speakers can't play ({e})")))?;
        let state = Arc::new(Mutex::new(Resample { rate, channels, src_rate, phase: 0.0, last: [0.0; 2] }));
        Ok(Self { _stream: stream, feeder: Feeder { ring, state } })
    }

    /// The handle sound is pushed through (any thread).
    pub fn feeder(&self) -> Feeder {
        self.feeder.clone()
    }

    /// Times the device asked for sound that wasn't there yet, and samples played.
    pub fn stats(&self) -> (u64, u64) {
        (self.feeder.ring.short.load(Ordering::Relaxed), self.feeder.ring.played.load(Ordering::Relaxed))
    }
}

impl Feeder {
    /// Plays interleaved stereo at the mixer's rate (turned to the device's rate and channels).
    pub fn push(&self, stereo: &[f32]) {
        let Ok(mut st) = self.state.lock() else { return };
        let st = &mut *st;
        let step = f64::from(st.src_rate) / f64::from(st.rate);
        let frames = stereo.len() / 2;
        let mut out = Vec::with_capacity((frames as f64 / step) as usize * st.channels + st.channels);
        // Linear between frames: plenty for hearing yourself, cheap enough for every block.
        while st.phase < frames as f64 {
            let i = st.phase.floor() as usize;
            let t = (st.phase - i as f64) as f32;
            let prev = if i == 0 { st.last } else { [stereo[(i - 1) * 2], stereo[(i - 1) * 2 + 1]] };
            let cur = [stereo[i * 2], stereo[i * 2 + 1]];
            let lr = [prev[0] + (cur[0] - prev[0]) * t, prev[1] + (cur[1] - prev[1]) * t];
            for c in 0..st.channels {
                out.push(match (st.channels, c) {
                    (1, _) => (lr[0] + lr[1]) * 0.5,
                    (_, 0) => lr[0],
                    (_, 1) => lr[1],
                    _ => 0.0,
                });
            }
            st.phase += step;
        }
        st.phase -= frames as f64;
        if frames > 0 {
            st.last = [stereo[(frames - 1) * 2], stereo[(frames - 1) * 2 + 1]];
        }
        if let Ok(mut q) = self.ring.samples.lock() {
            q.extend(out);
            let max = st.rate as usize * st.channels * MAX_MS / 1000;
            let over = q.len().saturating_sub(max);
            q.drain(..over);
        }
    }
}
