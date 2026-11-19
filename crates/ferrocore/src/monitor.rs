//! Hearing sound here (OBS's Audio Monitoring): the mixer's monitor mix played on an output
//! device (the system's, or one by name), through cpal. The engine pushes each block; the
//! device's callback takes what's there and plays silence when it runs short (counted).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::AvError;

/// What's kept waiting to be played: enough that a frame drawn a little late doesn't leave the
/// speakers empty (they crackled: nothing was kept, 18 Nov), little enough to hear yourself.
const TARGET_MS: usize = 40;
/// At most this much waits (a stalled device can't grow memory); over it, the queue goes back to
/// the target, not to here.
const MAX_MS: usize = 200;
/// The most the playback speed is nudged to keep the target (the speakers' clock and the
/// mixer's are never quite the same): 0.2%, which no one hears.
const STEER: f64 = 0.002;

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
    /// Playing (false: filling up to the target first, at the start and after running dry).
    primed: AtomicBool,
    /// The device stopped (unplugged, or the system's default changed): opened again.
    dead: AtomicBool,
}

/// Monitoring on a device; dropping it stops. Its [`Feeder`] can go to another thread. A small
/// thread of its own keeps the device: one that stops (unplugged, or headphones plugged in so
/// the system's speakers change) is opened again on the same choice; monitoring used to stop
/// for good until the Studio started again (owner's log, 7 times).
pub struct Monitor {
    feeder: Feeder,
    stop: Arc<AtomicBool>,
    keeper: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for Monitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Monitor").field("feeder", &self.feeder).finish_non_exhaustive()
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.keeper.take() {
            // Woken at once (it may be waiting seconds before trying the speakers again): the
            // window's thread drops this when Settings change.
            t.thread().unpark();
            let _ = t.join();
        }
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
    /// How full the queue is, smoothed (seconds).
    fill: f64,
}

/// Opens the stream on `device` (None: the system's) for `ring` → it, and the device's rate and
/// channels.
fn open(device: Option<&str>, ring: &Arc<Ring>) -> Result<(cpal::Stream, u32, usize), AvError> {
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
    let target = rate as usize * channels * TARGET_MS / 1000;
    let r = ring.clone();
    let dead = ring.clone();
    let stream = dev
        .build_output_stream(
            cfg.config(),
            move |out: &mut [f32], _| {
                // Never waits on the thread that feeds it: if the queue is busy this moment,
                // this callback plays silence.
                let Ok(mut q) = r.samples.try_lock() else {
                    out.fill(0.0);
                    r.short.fetch_add(1, Ordering::Relaxed);
                    return;
                };
                if !r.primed.load(Ordering::Relaxed) {
                    if q.len() < target {
                        out.fill(0.0);
                        return;
                    }
                    r.primed.store(true, Ordering::Relaxed);
                }
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
                    // Ran dry: fill up again before playing on (one gap, not a crackle).
                    r.short.fetch_add(1, Ordering::Relaxed);
                    r.primed.store(false, Ordering::Relaxed);
                }
            },
            move |e| match e.kind() {
                // A click (counted, as a run short) or the system moving the stream to new
                // speakers itself: it plays on, nothing to open again.
                cpal::ErrorKind::Xrun => {
                    dead.short.fetch_add(1, Ordering::Relaxed);
                }
                cpal::ErrorKind::DeviceChanged => tracing::info!(%e, "monitoring follows the new speakers"),
                _ => {
                    tracing::warn!(%e, "monitoring stopped: it's opened again");
                    dead.dead.store(true, Ordering::Release);
                }
            },
            None,
        )
        .map_err(|e| AvError::Source(format!("the speakers can't play ({e})")))?;
    stream.play().map_err(|e| AvError::Source(format!("the speakers can't play ({e})")))?;
    Ok((stream, rate, channels))
}

impl Monitor {
    /// Starts on `device` (a name from [`devices`]; None is the system's), for sound at
    /// `src_rate` (the mixer's).
    pub fn start(device: Option<&str>, src_rate: u32) -> Result<Self, AvError> {
        let ring = Arc::new(Ring {
            samples: Mutex::new(VecDeque::new()),
            short: AtomicU64::new(0),
            played: AtomicU64::new(0),
            primed: AtomicBool::new(false),
            dead: AtomicBool::new(false),
        });
        let (stream, rate, channels) = open(device, &ring)?;
        let state = Arc::new(Mutex::new(Resample {
            rate,
            channels,
            src_rate,
            phase: 0.0,
            last: [0.0; 2],
            fill: TARGET_MS as f64 / 1000.0,
        }));
        let feeder = Feeder { ring, state };
        let stop = Arc::new(AtomicBool::new(false));
        let keeper = {
            let (feeder, stop) = (feeder.clone(), stop.clone());
            let device = device.map(String::from);
            std::thread::Builder::new()
                .name("ferrocast-monitor".into())
                .spawn(move || keep(stream, device, feeder, stop))
                .map_err(|e| AvError::Source(format!("monitoring couldn't start ({e})")))?
        };
        Ok(Self { feeder, stop, keeper: Some(keeper) })
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

/// The keeper: holds the stream, and opens it again when it stops (waiting a little longer
/// after each try that fails).
fn keep(stream: cpal::Stream, device: Option<String>, feeder: Feeder, stop: Arc<AtomicBool>) {
    let mut stream = Some(stream);
    let mut wait = std::time::Duration::from_millis(500);
    // Waits, woken early when the monitor is dropped.
    let rest = |d: std::time::Duration| {
        let until = std::time::Instant::now() + d;
        while !stop.load(Ordering::Acquire) {
            let left = until.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            std::thread::park_timeout(left);
        }
    };
    while !stop.load(Ordering::Acquire) {
        rest(std::time::Duration::from_millis(100));
        if !feeder.ring.dead.load(Ordering::Acquire) {
            continue;
        }
        stream = None;
        rest(wait);
        if stop.load(Ordering::Acquire) {
            break;
        }
        feeder.ring.dead.store(false, Ordering::Release);
        match open(device.as_deref(), &feeder.ring) {
            Ok((s, rate, channels)) => {
                if let Ok(mut st) = feeder.state.lock() {
                    st.rate = rate;
                    st.channels = channels;
                    st.phase = 0.0;
                    st.fill = TARGET_MS as f64 / 1000.0;
                }
                if let Ok(mut q) = feeder.ring.samples.lock() {
                    q.clear();
                }
                feeder.ring.primed.store(false, Ordering::Release);
                tracing::info!(rate, channels, "monitoring plays again");
                stream = Some(s);
                wait = std::time::Duration::from_millis(500);
            }
            Err(e) => {
                tracing::warn!(%e, "monitoring couldn't open the speakers; it tries again");
                feeder.ring.dead.store(true, Ordering::Release);
                wait = (wait * 2).min(std::time::Duration::from_secs(10));
            }
        }
    }
    drop(stream);
}

impl Feeder {
    /// Plays interleaved stereo at the mixer's rate (turned to the device's rate and channels,
    /// a hair faster or slower to keep what waits near the target).
    pub fn push(&self, stereo: &[f32]) {
        let Ok(mut st) = self.state.lock() else { return };
        let st = &mut *st;
        let queued = self.ring.samples.lock().map(|q| q.len()).unwrap_or(0);
        let now = queued as f64 / (st.rate as f64 * st.channels.max(1) as f64);
        st.fill += (now - st.fill) * 0.05;
        let target = TARGET_MS as f64 / 1000.0;
        let nudge = ((st.fill - target) / target).clamp(-1.0, 1.0) * STEER;
        let step = f64::from(st.src_rate) / f64::from(st.rate) * (1.0 + nudge);
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
            let per_ms = st.rate as usize * st.channels / 1000;
            if q.len() > per_ms * MAX_MS {
                // Far behind (the device stalled): back to the target, a whole frame at a time.
                let over = (q.len() - per_ms * TARGET_MS) / st.channels.max(1) * st.channels.max(1);
                q.drain(..over);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Speakers that stop (unplugged, the system's default changed) are opened again and play
    /// on: monitoring used to stay silent until the Studio started again.
    #[test]
    fn stopped_speakers_are_opened_again() {
        if std::env::var_os("CI").is_some() {
            return;
        }
        let Ok(m) = Monitor::start(None, 48_000) else { return };
        let f = m.feeder();
        let feed = |ms: u64| {
            let until = std::time::Instant::now() + std::time::Duration::from_millis(ms);
            while std::time::Instant::now() < until {
                f.push(&[0.0f32; 2048]);
                std::thread::sleep(std::time::Duration::from_millis(21));
            }
        };
        feed(300);
        // As the error callback says when the device goes.
        m.feeder.ring.dead.store(true, Ordering::Release);
        feed(1500);
        assert!(!m.feeder.ring.dead.load(Ordering::Acquire), "opened again");
        let (_, before) = m.stats();
        feed(500);
        let (_, after) = m.stats();
        assert!(after > before + 10_000, "and it plays: {before} → {after}");
        // Dropped while it waits to try again: at once, not after its wait.
        m.feeder.ring.dead.store(true, Ordering::Release);
        std::thread::sleep(std::time::Duration::from_millis(150));
        let t = std::time::Instant::now();
        drop(m);
        assert!(t.elapsed() < std::time::Duration::from_millis(300), "stopped at once: {:?}", t.elapsed());
    }
}
