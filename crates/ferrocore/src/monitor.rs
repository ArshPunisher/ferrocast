//! Hearing sound here (OBS's Audio Monitoring): the mixer's monitor mix played on an output
//! device (the system's, or one by name), through cpal. The engine pushes each block into a
//! lock-free SPSC ring (zero allocations, zero mutex contention with the OS audio callback), and
//! a keeper thread opens the speakers again when disconnected — or waits for them if missing at
//! startup.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rubato::{Resampler as _, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction};

use crate::AvError;

/// What's kept waiting to be played: enough that a frame drawn a little late doesn't leave the
/// speakers empty (they crackled: nothing was kept, 18 Nov), little enough to hear yourself.
const TARGET_MS: usize = 40;
/// At most this much waits (a stalled device can't grow memory); over it, the consumer jumps back
/// to the target.
const MAX_MS: usize = 200;
/// Capacity of the lock-free SPSC ring in milliseconds.
const RING_MS: usize = 500;
/// The most the playback speed is nudged to keep the target (the speakers' clock and the
/// mixer's are never quite the same): 0.2%, which no one hears.
const STEER: f64 = 0.002;
/// Input frames per sinc resampler chunk (divides `mix::BLOCK = 1024` evenly).
const RESAMPLE_CHUNK: usize = 256;
const RETRY_START_MS: u64 = 500;
const RETRY_MAX_MS: u64 = 5_000;
const POLL_MS: u64 = 100;

/// Output devices here, by name.
pub fn devices() -> Vec<String> {
    let host = cpal::default_host();
    host.output_devices()
        .map(|d| d.filter_map(|d| d.description().ok().map(|n| n.to_string())).collect())
        .unwrap_or_default()
}

struct Stats {
    short: AtomicU64,
    played: AtomicU64,
    /// The device stopped (unplugged, or missing at startup): opened when available.
    dead: AtomicBool,
    /// Default device changed (macOS AudioUnit rerouted): keeper refreshes device id.
    device_changed: AtomicBool,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            short: AtomicU64::new(0),
            played: AtomicU64::new(0),
            dead: AtomicBool::new(false),
            device_changed: AtomicBool::new(false),
        }
    }
}

/// Lock-free single-producer, single-consumer ring buffer of `f32` samples (stored as bit
/// patterns in `AtomicU32` so no `unsafe` is needed). Only the producer writes `tail`, and only
/// the consumer writes `head` (except during stream setup before playback starts).
struct Ring {
    buf: Box<[AtomicU32]>,
    cap: usize,
    channels: usize,
    target_samples: usize,
    max_samples: usize,
    head: AtomicU64,
    tail: AtomicU64,
    /// Playing (false: filling up to the target first, at the start and after running dry).
    primed: AtomicBool,
    /// Deliberately draining to silence when monitoring is turned off (don't count as `short`).
    draining: AtomicBool,
    stats: Arc<Stats>,
}

impl Ring {
    fn empty(stats: Arc<Stats>) -> Self {
        Self {
            buf: Box::new([]),
            cap: 0,
            channels: 2,
            target_samples: 0,
            max_samples: 0,
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            primed: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            stats,
        }
    }

    fn new(rate: u32, channels: usize, stats: Arc<Stats>) -> Self {
        let rate = rate.max(1) as usize;
        let channels = channels.max(1);
        let target_frames = (rate * TARGET_MS / 1000).max(1);
        let max_frames = (rate * MAX_MS / 1000).max(target_frames + 1);
        let cap_frames = (rate * RING_MS / 1000).max(max_frames + 1024);
        let cap = cap_frames * channels;
        let buf: Box<[AtomicU32]> = (0..cap).map(|_| AtomicU32::new(0)).collect();
        Self {
            buf,
            cap,
            channels,
            target_samples: target_frames * channels,
            max_samples: max_frames * channels,
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            primed: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            stats,
        }
    }

    fn len(&self) -> usize {
        if self.cap == 0 {
            return 0;
        }
        self.tail.load(Ordering::Relaxed).saturating_sub(self.head.load(Ordering::Acquire)) as usize
    }

    fn is_primed(&self) -> bool {
        self.primed.load(Ordering::Acquire)
    }

    /// Producer only: writes stereo planar frames mapped to `self.channels`.
    fn push_planar(&self, left: &[f32], right: &[f32]) -> usize {
        if self.cap == 0 {
            return 0;
        }
        let h = self.head.load(Ordering::Acquire);
        let t = self.tail.load(Ordering::Relaxed);
        let used = t.saturating_sub(h) as usize;
        let free_frames = self.cap.saturating_sub(used) / self.channels;
        let frames = left.len().min(right.len()).min(free_frames);
        if frames == 0 {
            return 0;
        }
        let cap = self.cap as u64;
        let mut pos = t;
        for i in 0..frames {
            let (l, r) = (left[i], right[i]);
            match self.channels {
                1 => {
                    let m = (l + r) * 0.5;
                    self.buf[(pos % cap) as usize].store(m.to_bits(), Ordering::Relaxed);
                    pos += 1;
                }
                2 => {
                    self.buf[(pos % cap) as usize].store(l.to_bits(), Ordering::Relaxed);
                    self.buf[((pos + 1) % cap) as usize].store(r.to_bits(), Ordering::Relaxed);
                    pos += 2;
                }
                ch => {
                    self.buf[(pos % cap) as usize].store(l.to_bits(), Ordering::Relaxed);
                    self.buf[((pos + 1) % cap) as usize].store(r.to_bits(), Ordering::Relaxed);
                    for c in 2..ch {
                        self.buf[((pos + c as u64) % cap) as usize].store(0, Ordering::Relaxed);
                    }
                    pos += ch as u64;
                }
            }
        }
        self.tail.store(pos, Ordering::Release);
        frames
    }

    /// Consumer only (OS audio callback): lock-free pull into `out`.
    fn pull(&self, out: &mut [f32]) {
        if self.cap == 0 || out.is_empty() {
            out.fill(0.0);
            return;
        }
        let t = self.tail.load(Ordering::Acquire);
        let mut h = self.head.load(Ordering::Relaxed);
        let mut avail = t.saturating_sub(h) as usize;
        if avail > self.max_samples {
            // Far behind (the device stalled): consumer advances back to target, whole frames.
            let drop_n = ((avail - self.target_samples) / self.channels) * self.channels;
            h += drop_n as u64;
            avail -= drop_n;
        }
        if !self.primed.load(Ordering::Relaxed) {
            if self.draining.load(Ordering::Acquire) {
                self.head.store(t, Ordering::Release);
                self.draining.store(false, Ordering::Release);
                out.fill(0.0);
                return;
            }
            if avail < self.target_samples {
                self.head.store(h, Ordering::Release);
                out.fill(0.0);
                return;
            }
            self.primed.store(true, Ordering::Relaxed);
        }
        let take = avail.min(out.len());
        let cap = self.cap as u64;
        for (i, dst) in out[..take].iter_mut().enumerate() {
            *dst = f32::from_bits(self.buf[((h + i as u64) % cap) as usize].load(Ordering::Relaxed));
        }
        self.head.store(h + take as u64, Ordering::Release);
        self.stats.played.fetch_add(out.len() as u64, Ordering::Relaxed);
        if take < out.len() {
            out[take..].fill(0.0);
            // Ran dry: fill up again before playing on (one gap, not a crackle).
            self.primed.store(false, Ordering::Relaxed);
            if !self.draining.swap(false, Ordering::AcqRel) {
                self.stats.short.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

struct DeviceSlot {
    epoch: AtomicU64,
    id: Mutex<Option<String>>,
}

impl DeviceSlot {
    fn new(id: Option<String>) -> Self {
        Self { epoch: AtomicU64::new(u64::from(id.is_some())), id: Mutex::new(id) }
    }

    fn set(&self, id: Option<String>) {
        if let Ok(mut g) = self.id.lock()
            && *g != id
        {
            *g = id;
            self.epoch.fetch_add(1, Ordering::Release);
        }
    }

    fn get(&self) -> Option<String> {
        self.id.lock().ok().and_then(|g| g.clone())
    }
}

/// Monitoring on a device; dropping it stops. Its [`Feeder`] can go to another thread. A small
/// thread of its own keeps the device: one that stops (unplugged, or headphones plugged in so
/// the system's speakers change) or is missing at startup is opened when available; monitoring
/// used to stop for good until the Studio started again (owner's log, 7 times).
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
    stats: Arc<Stats>,
    state: Arc<Mutex<Producer>>,
    device: Arc<DeviceSlot>,
    idle_skipped: Arc<AtomicBool>,
}

impl std::fmt::Debug for Feeder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Feeder").finish_non_exhaustive()
    }
}

/// Pre-allocated sinc resampler + scratch buffers so `Feeder::push` performs zero allocations.
struct MonitorResampler {
    sinc: SincFixedIn<f32>,
    pending: [Vec<f32>; 2],
    out_buf: [Vec<f32>; 2],
}

impl MonitorResampler {
    fn new(src_rate: u32, dst_rate: u32) -> Result<Self, AvError> {
        let ratio = f64::from(dst_rate.max(1)) / f64::from(src_rate.max(1));
        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            oversampling_factor: 128,
            interpolation: SincInterpolationType::Cubic,
            window: WindowFunction::BlackmanHarris2,
        };
        let sinc = SincFixedIn::<f32>::new(ratio, 1.05, params, RESAMPLE_CHUNK, 2)
            .map_err(|e| AvError::Source(format!("monitoring resampler failed ({e})")))?;
        let out_max = sinc.output_frames_max();
        Ok(Self {
            sinc,
            pending: [Vec::with_capacity(RESAMPLE_CHUNK), Vec::with_capacity(RESAMPLE_CHUNK)],
            out_buf: [vec![0.0; out_max], vec![0.0; out_max]],
        })
    }
}

/// Producer-only state (never touched by the OS audio callback).
struct Producer {
    ring: Arc<Ring>,
    rate: u32,
    channels: usize,
    src_rate: u32,
    /// How full the queue is, smoothed (seconds).
    fill: f64,
    resampler: MonitorResampler,
    was_idle: bool,
}

struct Opened {
    rate: u32,
    channels: usize,
    device_id: String,
    ring: Arc<Ring>,
    play: Box<dyn FnOnce() -> Result<Box<dyn Send>, AvError> + Send>,
}

type Opener = Box<dyn Fn(Option<&str>, &Arc<Stats>) -> Result<Opened, AvError> + Send>;

fn default_output_device_id() -> Option<String> {
    cpal::default_host().default_output_device().and_then(|d| d.id().ok()).map(|i| i.to_string())
}

/// Prepares the cpal stream on `device` (None: the system's) without starting playback yet, so
/// the keeper can swap the producer's ring and resampler before the callback begins pulling.
fn open_cpal(device: Option<&str>, stats: &Arc<Stats>) -> Result<Opened, AvError> {
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
    let device_id = dev.id().map(|i| i.to_string()).unwrap_or_else(|_| "default".into());
    let cfg = dev.default_output_config().map_err(|e| AvError::Source(format!("the speakers can't play ({e})")))?;
    let (rate, channels) = (cfg.sample_rate(), usize::from(cfg.channels()).max(1));
    let ring = Arc::new(Ring::new(rate, channels, stats.clone()));
    let r = ring.clone();
    let err_stats = stats.clone();
    let stream = dev
        .build_output_stream(
            cfg.config(),
            move |out: &mut [f32], _| r.pull(out),
            move |e| match e.kind() {
                cpal::ErrorKind::Xrun => {
                    err_stats.short.fetch_add(1, Ordering::Relaxed);
                }
                cpal::ErrorKind::DeviceChanged => {
                    tracing::info!(%e, "monitoring follows the new speakers");
                    err_stats.device_changed.store(true, Ordering::Release);
                }
                _ => {
                    tracing::warn!(%e, "monitoring stopped: it's opened again");
                    err_stats.dead.store(true, Ordering::Release);
                }
            },
            None,
        )
        .map_err(|e| AvError::Source(format!("the speakers can't play ({e})")))?;
    Ok(Opened {
        rate,
        channels,
        device_id,
        ring,
        play: Box::new(move || {
            stream.play().map_err(|e| AvError::Source(format!("the speakers can't play ({e})")))?;
            Ok(Box::new(stream))
        }),
    })
}

impl Monitor {
    /// Starts on `device` (a name from [`devices`]; None is the system's), for sound at
    /// `src_rate` (the mixer's). If the device isn't connected yet, starts waiting for it instead
    /// of failing so plugging headphones in later activates monitoring automatically.
    pub fn start(device: Option<&str>, src_rate: u32) -> Result<Self, AvError> {
        Self::start_with(
            device,
            src_rate,
            Duration::from_millis(POLL_MS),
            Duration::from_millis(RETRY_START_MS),
            Duration::from_millis(RETRY_MAX_MS),
            Box::new(open_cpal),
        )
    }

    fn start_with(
        device: Option<&str>,
        src_rate: u32,
        poll_every: Duration,
        retry_start: Duration,
        retry_max: Duration,
        opener: Opener,
    ) -> Result<Self, AvError> {
        if src_rate == 0 {
            return Err(AvError::Source("monitoring sample rate can't be 0".into()));
        }
        let stats = Arc::new(Stats::default());
        let initial = opener(device, &stats);
        let (stream, ring, rate, channels, dev_id, dead) = match initial {
            Ok(opened) => {
                let (rate, channels, dev_id, ring) = (opened.rate, opened.channels, opened.device_id, opened.ring);
                match (opened.play)() {
                    Ok(s) => (Some(s), ring, rate, channels, Some(dev_id), false),
                    Err(e) => {
                        tracing::warn!(%e, "monitoring speakers couldn't start yet; waiting to retry");
                        (None, Arc::new(Ring::empty(stats.clone())), src_rate, 2, None, true)
                    }
                }
            }
            Err(e) => {
                tracing::warn!(%e, "monitoring speakers aren't connected yet; waiting for them");
                (None, Arc::new(Ring::empty(stats.clone())), src_rate, 2, None, true)
            }
        };
        if dead {
            stats.dead.store(true, Ordering::Release);
        }
        let resampler = MonitorResampler::new(src_rate, rate)?;
        let state = Arc::new(Mutex::new(Producer {
            ring,
            rate,
            channels,
            src_rate,
            fill: TARGET_MS as f64 / 1000.0,
            resampler,
            was_idle: false,
        }));
        let device_slot = Arc::new(DeviceSlot::new(dev_id));
        let feeder = Feeder { stats, state, device: device_slot, idle_skipped: Arc::new(AtomicBool::new(false)) };
        let stop = Arc::new(AtomicBool::new(false));
        let keeper = {
            let (feeder, stop) = (feeder.clone(), stop.clone());
            let device = device.map(String::from);
            std::thread::Builder::new()
                .name("ferrocast-monitor".into())
                .spawn(move || keep(stream, device, feeder, stop, opener, poll_every, retry_start, retry_max))
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
        (self.feeder.stats.short.load(Ordering::Relaxed), self.feeder.stats.played.load(Ordering::Relaxed))
    }
}

/// The keeper: holds the stream, and opens it again when it stops or appears (waiting a little
/// longer after each try that fails).
#[allow(clippy::too_many_arguments)]
fn keep(
    mut stream: Option<Box<dyn Send>>,
    device: Option<String>,
    feeder: Feeder,
    stop: Arc<AtomicBool>,
    opener: Opener,
    poll_every: Duration,
    retry_start: Duration,
    retry_max: Duration,
) {
    let mut wait = retry_start;
    // Waits, woken early when the monitor is dropped.
    let rest = |d: Duration| {
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
        if stream.is_some() {
            rest(poll_every);
            if stop.load(Ordering::Acquire) {
                break;
            }
            if feeder.stats.device_changed.swap(false, Ordering::AcqRel) && device.is_none() {
                feeder.device.set(default_output_device_id());
            }
            if !feeder.stats.dead.load(Ordering::Acquire) {
                continue;
            }
            stream = None;
            feeder.device.set(None);
        }
        rest(wait);
        if stop.load(Ordering::Acquire) {
            break;
        }
        match opener(device.as_deref(), &feeder.stats) {
            Ok(opened) => {
                let src_rate = feeder.state.lock().map(|p| p.src_rate).unwrap_or(48_000);
                let Ok(resampler) = MonitorResampler::new(src_rate, opened.rate) else {
                    feeder.stats.dead.store(true, Ordering::Release);
                    wait = (wait * 2).min(retry_max);
                    continue;
                };
                if let Ok(mut prod) = feeder.state.lock() {
                    prod.ring = opened.ring;
                    prod.rate = opened.rate;
                    prod.channels = opened.channels;
                    prod.fill = TARGET_MS as f64 / 1000.0;
                    prod.resampler = resampler;
                    prod.was_idle = false;
                }
                feeder.idle_skipped.store(false, Ordering::Release);
                feeder.stats.dead.store(false, Ordering::Release);
                match (opened.play)() {
                    Ok(s) => {
                        feeder.device.set(Some(opened.device_id));
                        tracing::info!(rate = opened.rate, channels = opened.channels, "monitoring plays again");
                        stream = Some(s);
                        wait = retry_start;
                    }
                    Err(e) => {
                        tracing::warn!(%e, "monitoring couldn't start the speakers; it tries again");
                        feeder.stats.dead.store(true, Ordering::Release);
                        feeder.device.set(None);
                        wait = (wait * 2).min(retry_max);
                    }
                }
            }
            Err(e) => {
                tracing::warn!(%e, "monitoring couldn't open the speakers; it tries again");
                feeder.stats.dead.store(true, Ordering::Release);
                wait = (wait * 2).min(retry_max);
            }
        }
    }
    drop(stream);
}

impl Feeder {
    /// Monotonically incremented when the active output device changes, opens, or closes.
    pub fn device_epoch(&self) -> u64 {
        self.device.epoch.load(Ordering::Acquire)
    }

    /// Resolved `cpal::DeviceId` string of the active output device (`None` while disconnected).
    pub fn device_id(&self) -> Option<String> {
        self.device.get()
    }

    /// Marks monitoring idle when no active source is monitored, draining any queued tail
    /// cleanly without counting an underrun and skipping work while silent.
    pub fn idle(&self) {
        if self.stats.dead.load(Ordering::Acquire) || self.idle_skipped.load(Ordering::Acquire) {
            return;
        }
        let Ok(mut prod) = self.state.lock() else { return };
        if prod.ring.cap == 0 {
            return;
        }
        if prod.ring.is_primed() || prod.ring.len() > 0 {
            prod.ring.draining.store(true, Ordering::Release);
        } else {
            self.idle_skipped.store(true, Ordering::Release);
        }
        prod.was_idle = true;
    }

    /// Plays interleaved stereo at the mixer's rate (turned to the device's rate and channels,
    /// a hair faster or slower to keep what waits near the target).
    pub fn push(&self, stereo: &[f32]) {
        if self.stats.dead.load(Ordering::Acquire) {
            return;
        }
        let frames = stereo.len() / 2;
        if frames == 0 {
            return;
        }
        self.idle_skipped.store(false, Ordering::Release);
        let Ok(mut prod) = self.state.lock() else { return };
        let prod = &mut *prod;
        if prod.ring.cap == 0 {
            return;
        }
        if prod.was_idle {
            prod.was_idle = false;
            prod.resampler.sinc.reset();
            prod.resampler.pending[0].clear();
            prod.resampler.pending[1].clear();
            prod.fill = TARGET_MS as f64 / 1000.0;
        }
        prod.ring.draining.store(false, Ordering::Release);
        let queued = prod.ring.len();
        let now = queued as f64 / (prod.rate.max(1) as f64 * prod.channels.max(1) as f64);
        prod.fill += (now - prod.fill) * 0.05;
        let target = TARGET_MS as f64 / 1000.0;
        let nudge = ((prod.fill - target) / target).clamp(-1.0, 1.0) * STEER;
        let _ = prod.resampler.sinc.set_resample_ratio_relative(1.0 / (1.0 + nudge), true);
        let mut idx = 0usize;
        while idx < frames {
            let need = (RESAMPLE_CHUNK - prod.resampler.pending[0].len()).min(frames - idx);
            for f in stereo[idx * 2..(idx + need) * 2].as_chunks::<2>().0 {
                prod.resampler.pending[0].push(if f[0].is_finite() { f[0] } else { 0.0 });
                prod.resampler.pending[1].push(if f[1].is_finite() { f[1] } else { 0.0 });
            }
            idx += need;
            if prod.resampler.pending[0].len() == RESAMPLE_CHUNK {
                let res = {
                    let r = &mut prod.resampler;
                    let inp = [&r.pending[0][..RESAMPLE_CHUNK], &r.pending[1][..RESAMPLE_CHUNK]];
                    r.sinc.process_into_buffer(&inp, &mut r.out_buf, None)
                };
                prod.resampler.pending[0].clear();
                prod.resampler.pending[1].clear();
                if let Ok((_, out_frames)) = res
                    && out_frames > 0
                {
                    prod.ring.push_planar(
                        &prod.resampler.out_buf[0][..out_frames],
                        &prod.resampler.out_buf[1][..out_frames],
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_feeder(src_rate: u32, dst_rate: u32, channels: usize) -> (Feeder, Arc<Ring>) {
        let stats = Arc::new(Stats::default());
        let ring = Arc::new(Ring::new(dst_rate, channels, stats.clone()));
        let resampler = MonitorResampler::new(src_rate, dst_rate).unwrap_or_else(|e| panic!("{e}"));
        let state = Arc::new(Mutex::new(Producer {
            ring: ring.clone(),
            rate: dst_rate,
            channels,
            src_rate,
            fill: TARGET_MS as f64 / 1000.0,
            resampler,
            was_idle: false,
        }));
        let feeder = Feeder {
            stats,
            state,
            device: Arc::new(DeviceSlot::new(Some("test".into()))),
            idle_skipped: Arc::new(AtomicBool::new(false)),
        };
        (feeder, ring)
    }

    /// Speakers that stop (unplugged, the system's default changed) are opened again and play
    /// on: monitoring used to stay silent until the Studio started again.
    #[test]
    fn stopped_speakers_are_opened_again() {
        if std::env::var_os("CI").is_some() {
            return;
        }
        let Ok(m) = Monitor::start(None, 48_000) else { return };
        let f = m.feeder();
        if f.stats.dead.load(Ordering::Acquire) {
            return; // no speakers here
        }
        let feed = |ms: u64| {
            let until = std::time::Instant::now() + std::time::Duration::from_millis(ms);
            while std::time::Instant::now() < until {
                f.push(&[0.0f32; 2048]);
                std::thread::sleep(std::time::Duration::from_millis(21));
            }
        };
        feed(300);
        // As the error callback says when the device goes.
        m.feeder.stats.dead.store(true, Ordering::Release);
        feed(1500);
        assert!(!m.feeder.stats.dead.load(Ordering::Acquire), "opened again");
        let (_, before) = m.stats();
        feed(500);
        let (_, after) = m.stats();
        assert!(after > before + 10_000, "and it plays: {before} → {after}");
        // Dropped while it waits to try again: at once, not after its wait.
        m.feeder.stats.dead.store(true, Ordering::Release);
        std::thread::sleep(std::time::Duration::from_millis(150));
        let t = std::time::Instant::now();
        drop(m);
        assert!(t.elapsed() < std::time::Duration::from_millis(300), "stopped at once: {:?}", t.elapsed());
    }

    #[test]
    fn missing_device_at_start_still_starts_keeper() {
        let m = Monitor::start(Some("__ferrocast_missing_monitor_device__"), 48_000);
        assert!(m.is_ok(), "missing device at start must start retrying instead of failing: {m:?}");
    }

    #[test]
    fn missing_device_opens_once_it_connects_and_plays() {
        use std::sync::atomic::AtomicUsize;
        let attempts = Arc::new(AtomicUsize::new(0));
        let opened_ring: Arc<Mutex<Option<Arc<Ring>>>> = Arc::new(Mutex::new(None));
        let attempts_c = attempts.clone();
        let ring_slot = opened_ring.clone();
        let opener: Opener = Box::new(move |_dev, stats| {
            let n = attempts_c.fetch_add(1, Ordering::SeqCst) + 1;
            if n < 3 {
                return Err(AvError::Source("disconnected".into()));
            }
            let ring = Arc::new(Ring::new(48_000, 2, stats.clone()));
            if let Ok(mut g) = ring_slot.lock() {
                *g = Some(ring.clone());
            }
            Ok(Opened {
                rate: 48_000,
                channels: 2,
                device_id: "usb-dac".into(),
                ring,
                play: Box::new(|| Ok(Box::new(()))),
            })
        });
        let m = Monitor::start_with(
            Some("USB DAC"),
            48_000,
            Duration::from_millis(10),
            Duration::from_millis(15),
            Duration::from_millis(30),
            opener,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let feeder = m.feeder();
        assert!(feeder.device_id().is_none(), "offline initially");
        let until = std::time::Instant::now() + Duration::from_secs(2);
        while feeder.device_id().is_none() && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(feeder.device_id().as_deref(), Some("usb-dac"), "keeper opened device once connected");
        let ring = opened_ring.lock().ok().and_then(|g| g.clone()).unwrap_or_else(|| panic!("ring"));
        for _ in 0..6 {
            feeder.push(&[0.25f32; 2048]);
        }
        let mut out = [0.0f32; 512];
        let mut nonzero = 0usize;
        for _ in 0..10 {
            ring.pull(&mut out);
            nonzero += out.iter().filter(|v| v.abs() > 0.1).count();
        }
        assert!(nonzero > 1000, "audio plays once recovered: {nonzero}");
    }

    #[test]
    fn concurrent_push_and_pull_never_drops_for_lock_contention() {
        let rate = 48_000u32;
        let channels = 2usize;
        let target = rate as usize * channels * TARGET_MS / 1000;
        let max_samples = rate as usize * channels * MAX_MS / 1000;
        let (feeder, ring) = test_feeder(rate, rate, channels);
        let initial = target * 2;
        let half = vec![0.1f32; initial / 2];
        ring.push_planar(&half, &half);
        let produced = Arc::new(AtomicU64::new(initial as u64));
        let consumed = Arc::new(AtomicU64::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let (done_c, ring_c, prod_c, cons_c) = (done.clone(), ring.clone(), produced.clone(), consumed.clone());
        let consumer = std::thread::spawn(move || {
            let mut out = [0.0f32; 256];
            while !done_c.load(Ordering::Acquire) {
                let p = prod_c.load(Ordering::Acquire);
                let c = cons_c.load(Ordering::Relaxed);
                if p.saturating_sub(c) >= target as u64 + 256 {
                    let prev_short = ring_c.stats.short.load(Ordering::Relaxed);
                    ring_c.pull(&mut out);
                    if ring_c.stats.short.load(Ordering::Relaxed) == prev_short {
                        cons_c.fetch_add(256, Ordering::Release);
                    }
                } else {
                    std::thread::yield_now();
                }
            }
        });
        let block = [0.1f32; 2048];
        for _ in 0..1200 {
            while produced.load(Ordering::Relaxed).saturating_sub(consumed.load(Ordering::Acquire))
                > (max_samples - 2048) as u64
            {
                std::thread::yield_now();
            }
            let t0 = ring.tail.load(Ordering::Relaxed);
            feeder.push(&block);
            let t1 = ring.tail.load(Ordering::Relaxed);
            produced.fetch_add(t1.saturating_sub(t0), Ordering::Release);
        }
        done.store(true, Ordering::Release);
        let _ = consumer.join();
        let short = ring.stats.short.load(Ordering::Relaxed);
        assert_eq!(short, 0, "lock contention between Feeder::push and callback caused {short} dropouts");
    }

    #[test]
    fn feeder_sinc_resampler_rejects_aliasing() {
        let src_rate = 48_000u32;
        let dst_rate = 16_000u32;
        let (feeder, ring) = test_feeder(src_rate, dst_rate, 2);
        // 15 kHz tone at 48 kHz: above 16 kHz Nyquist (8 kHz), linear aliases to 1 kHz.
        let mut out = Vec::new();
        let mut phase = 0usize;
        for _ in 0..16 {
            let mut blk = Vec::with_capacity(2048);
            for _ in 0..1024 {
                let v = (phase as f32 * 15_000.0 * std::f32::consts::TAU / src_rate as f32).sin() * 0.5;
                blk.push(v);
                blk.push(v);
                phase += 1;
            }
            feeder.push(&blk);
            while ring.len() >= ring.target_samples.max(256) {
                let mut tmp = [0.0f32; 256];
                ring.pull(&mut tmp);
                out.extend_from_slice(&tmp);
            }
        }
        assert!(out.len() > 1000, "enough resampled output collected: {}", out.len());
        let steady = &out[out.len() / 2..];
        let peak = steady.iter().fold(0.0f32, |a, v| a.max(v.abs()));
        assert!(peak < 0.02, "15 kHz aliased through resampler: steady peak {peak}");
    }

    #[test]
    fn feeder_push_reuses_buffers_and_idle_drains_without_underrun() {
        let (feeder, ring) = test_feeder(48_000, 48_000, 2);
        let block = [0.2f32; 2048];
        feeder.push(&block);
        let (p0, o0) = {
            let prod = feeder.state.lock().unwrap_or_else(|e| e.into_inner());
            (
                (prod.resampler.pending[0].as_ptr(), prod.resampler.pending[0].capacity()),
                (prod.resampler.out_buf[0].as_ptr(), prod.resampler.out_buf[0].capacity()),
            )
        };
        for _ in 0..200 {
            feeder.push(&block);
            while ring.len() >= ring.target_samples.max(256) {
                let mut tmp = [0.0f32; 256];
                ring.pull(&mut tmp);
            }
        }
        let (p1, o1) = {
            let prod = feeder.state.lock().unwrap_or_else(|e| e.into_inner());
            (
                (prod.resampler.pending[0].as_ptr(), prod.resampler.pending[0].capacity()),
                (prod.resampler.out_buf[0].as_ptr(), prod.resampler.out_buf[0].capacity()),
            )
        };
        assert_eq!(p0, p1, "pending buffer never reallocated");
        assert_eq!(o0, o1, "output scratch buffer never reallocated");

        // Turning monitoring off drains cleanly without counting an underrun (`short`).
        let short_before = ring.stats.short.load(Ordering::Relaxed);
        feeder.idle();
        let mut tmp = [0.0f32; 512];
        for _ in 0..20 {
            ring.pull(&mut tmp);
        }
        assert_eq!(ring.stats.short.load(Ordering::Relaxed), short_before, "idle drain never counts short");
        assert!(!ring.is_primed());
        feeder.idle();
        assert!(feeder.idle_skipped.load(Ordering::Acquire), "subsequent idle calls skip without work");
    }
}
