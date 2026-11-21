//! Dedicated real-time audio mixing thread (`ferrocast-audio`, D1 Commit 3).
//!
//! `Mixer`, `Tone` generation, `Monitor::Feeder`, and audio `Tap`s are thread-confined to
//! `AudioRunner<C: AudioClock>`, ticking every 1024 frames (`21.333 ms` at 48 kHz) independently
//! of the video compositor and GPU readback stalls. Heavy `AudioFilter` plugins (`Denoise`, VST)
//! are constructed and dropped on a dedicated background worker (`ferrocast-afx-bg`) so real-time
//! audio ticks never block.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak, mpsc};
use std::thread::JoinHandle;

use crate::afx::{AudioEffect, AudioFilter, SoundPlugin};
use crate::level::Ducking;
use crate::mix::{BLOCK, Block, Mixer, OutputCapture, SourceSettings, push_tap_interleaved};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use crate::monitor::Feeder;
use crate::rt::{AtomicLevels, AudioClock, AudioConsumer, PopError, SystemClock};

/// Maximum catch-up horizon for tone generators and audio thread pacing after OS sleep (ns).
const MAX_CATCH_UP_NS: u64 = 2_000_000_000;
const MAX_TICK_CATCH_UP_NS: u64 = 1_000_000_000;
/// Temporary Commit 3 bridge capacity (~5.4 s of 1024-frame blocks at 48 kHz).
const BRIDGE_CAP: usize = 256;
/// Block pool return channel capacity (matches `mix::MAX_POOL * 2`).
const RECYCLE_CAP: usize = 128;
/// Initial pre-sized scratch capacity when popping pieces from per-source SPSC rings.
const INITIAL_RING_SCRATCH_SAMPLES: usize = 48_000 * 2;

/// Definition of a synthesized test tone source owned by the audio thread.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToneSpec {
    pub hz: f32,
    pub every_ns: u64,
    pub beep_ns: u64,
}

struct ToneState {
    spec: ToneSpec,
    pushed_to: Option<u64>,
}

/// Commands sent from `Engine` / control threads / `ferrocast-afx-bg` to `ferrocast-audio`.
#[allow(clippy::large_enum_variant)] // rare messages; boxing would free on the audio thread
pub enum AudioCmd {
    /// A source entry made ahead (see `AfxJob::MakeSpare`).
    Spare(crate::mix::SpareSource),
    /// Sound at this input rate is coming: have a resampler made.
    WantResampler(u32),
    /// A resampler made ahead (see `AfxJob::MakeResampler`).
    SpareResampler(crate::mix::SpareResampler),
    EnsureSource {
        id: u64,
        levels: Arc<AtomicLevels>,
    },
    RemoveSource(u64),
    ScheduleRemove {
        id: u64,
        after_ns: u64,
    },
    AttachRing {
        id: u64,
        consumer: AudioConsumer,
    },
    FlushSource(u64),
    SetTone {
        id: u64,
        tone: Option<ToneSpec>,
        levels: Option<Arc<AtomicLevels>>,
    },
    SetSettings {
        id: u64,
        settings: SourceSettings,
        levels: Arc<AtomicLevels>,
    },
    SetOutputCapture {
        id: u64,
        capture: Option<OutputCapture>,
    },
    /// A push-to-talk (`talk`) or push-to-mute key went down or up.
    PushKey {
        id: u64,
        talk: bool,
        down: bool,
    },
    SetEffects {
        id: u64,
        effects: Vec<AudioEffect>,
        levels: Option<Arc<AtomicLevels>>,
    },
    AttachPlugins {
        id: u64,
        built: Vec<(u64, AudioEffect, Box<dyn SoundPlugin>)>,
    },
    SetHeard(Arc<[u64]>),
    SetLowLatency(bool),
    SetTruePeak(bool),
    SetDucking(Option<Ducking>),
    SetLoudness(Option<f32>),
    AttachTap {
        id: u64,
        tap: Weak<Mutex<VecDeque<f32>>>,
    },
    AttachMasterTap(Weak<Mutex<VecDeque<f32>>>),
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    SetMonitor {
        feeder: Option<Feeder>,
        dev_id: Option<String>,
        epoch: u64,
    },
    #[cfg(test)]
    QuerySettings {
        id: u64,
        reply: mpsc::SyncSender<Option<SourceSettings>>,
    },
}

impl std::fmt::Debug for AudioCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spare(_) => f.write_str("Spare"),
            Self::WantResampler(r) => f.debug_tuple("WantResampler").field(r).finish(),
            Self::SpareResampler(_) => f.write_str("SpareResampler"),
            Self::EnsureSource { id, .. } => f.debug_struct("EnsureSource").field("id", id).finish(),
            Self::RemoveSource(id) => f.debug_tuple("RemoveSource").field(id).finish(),
            Self::ScheduleRemove { id, after_ns } => {
                f.debug_struct("ScheduleRemove").field("id", id).field("after_ns", after_ns).finish()
            }
            Self::AttachRing { id, .. } => f.debug_struct("AttachRing").field("id", id).finish(),
            Self::FlushSource(id) => f.debug_tuple("FlushSource").field(id).finish(),
            Self::SetTone { id, tone, .. } => f.debug_struct("SetTone").field("id", id).field("tone", tone).finish(),
            Self::SetSettings { id, settings, .. } => {
                f.debug_struct("SetSettings").field("id", id).field("settings", settings).finish()
            }
            Self::PushKey { id, talk, down } => {
                f.debug_struct("PushKey").field("id", id).field("talk", talk).field("down", down).finish()
            }
            Self::SetOutputCapture { id, capture } => {
                f.debug_struct("SetOutputCapture").field("id", id).field("capture", capture).finish()
            }
            Self::SetEffects { id, effects, .. } => {
                f.debug_struct("SetEffects").field("id", id).field("effects_len", &effects.len()).finish()
            }
            Self::AttachPlugins { id, built } => {
                f.debug_struct("AttachPlugins").field("id", id).field("count", &built.len()).finish()
            }
            Self::SetHeard(h) => f.debug_tuple("SetHeard").field(&h.len()).finish(),
            Self::SetLowLatency(on) => f.debug_tuple("SetLowLatency").field(on).finish(),
            Self::SetTruePeak(on) => f.debug_tuple("SetTruePeak").field(on).finish(),
            Self::SetDucking(d) => f.debug_tuple("SetDucking").field(d).finish(),
            Self::SetLoudness(l) => f.debug_tuple("SetLoudness").field(l).finish(),
            Self::AttachTap { id, .. } => f.debug_struct("AttachTap").field("id", id).finish(),
            Self::AttachMasterTap(_) => f.write_str("AttachMasterTap"),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            Self::SetMonitor { dev_id, epoch, .. } => {
                f.debug_struct("SetMonitor").field("dev_id", dev_id).field("epoch", epoch).finish()
            }
            #[cfg(test)]
            Self::QuerySettings { id, .. } => f.debug_struct("QuerySettings").field("id", id).finish(),
        }
    }
}

/// Jobs dispatched from `ferrocast-audio` to `ferrocast-afx-bg` so heavy plugin setup and teardown
/// never happen on the real-time audio thread.
#[allow(clippy::large_enum_variant)] // as AudioCmd
pub(crate) enum AfxJob {
    Build {
        id: u64,
        rate: u32,
        items: Vec<(u64, AudioEffect)>,
    },
    DropFilters(Vec<AudioFilter>),
    /// The list of heard sources a newer one replaced.
    DropHeard(Arc<[u64]>),
    /// A removed source's mixer entry (its buffers free here, not on the audio thread).
    DropSource(crate::mix::SpareSource),
    /// A source's level meter, when the audio thread held its last reference.
    DropLevels(Arc<AtomicLevels>),
    /// A ring a source left or replaced.
    DropRing(AudioConsumer),
    /// A resampler a source stopped using.
    DropResampler(crate::mix::SpareResampler),
    /// Make a resampler from `in_rate` to `out_rate` ahead (same rate: what following drift
    /// needs; another: a source at that rate has appeared).
    MakeResampler {
        in_rate: u32,
        out_rate: u32,
    },
    /// Make `n` source entries ahead for sources that appear.
    MakeSpare {
        rate: u32,
        n: usize,
    },
    DropPlugins(Vec<Box<dyn SoundPlugin>>),
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    DropFeeder(Feeder),
}

/// Jobs that may wait for the background thread. A bounded channel, so the audio thread's `try_send`
/// never allocates (an unbounded one allocates a block every few dozen messages); if it were ever
/// full, the job is dropped there (its memory frees on the audio thread: a rare click, not a stall).
const AFX_QUEUE: usize = 256;

type PluginBuilder = fn(AudioEffect, u32) -> Option<Box<dyn SoundPlugin>>;

fn run_afx_bg(rx: mpsc::Receiver<AfxJob>, cmd_tx: mpsc::Sender<AudioCmd>, builder: PluginBuilder) {
    while let Ok(job) = rx.recv() {
        match job {
            AfxJob::Build { id, rate, items } => {
                let mut built = Vec::with_capacity(items.len());
                for (build_id, effect) in items {
                    if let Some(plugin) = builder(effect, rate) {
                        built.push((build_id, effect, plugin));
                    }
                }
                if !built.is_empty() {
                    let _ = cmd_tx.send(AudioCmd::AttachPlugins { id, built });
                }
            }
            AfxJob::DropFilters(filters) => drop(filters),
            AfxJob::DropHeard(old) => drop(old),
            AfxJob::DropSource(src) => drop(src),
            AfxJob::DropRing(ring) => drop(ring),
            AfxJob::DropLevels(l) => drop(l),
            AfxJob::DropResampler(r) => drop(r),
            AfxJob::MakeResampler { in_rate, out_rate } => {
                if let Some(r) = Mixer::make_spare_resampler(in_rate, out_rate) {
                    let _ = cmd_tx.send(AudioCmd::SpareResampler(r));
                }
            }
            AfxJob::MakeSpare { rate, n } => {
                for _ in 0..n {
                    let _ = cmd_tx.send(AudioCmd::Spare(Mixer::make_spare(rate)));
                }
            }
            AfxJob::DropPlugins(plugins) => drop(plugins),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            AfxJob::DropFeeder(feeder) => drop(feeder),
        }
    }
}

/// The last blocks' mixing times and wake errors, in microseconds, in fixed arrays.
struct TimingWindow {
    mix_us: [u32; Self::LEN],
    late_us: [u32; Self::LEN],
    n: usize,
}

impl TimingWindow {
    /// About 5.5 s of 21.3 ms blocks.
    const LEN: usize = 256;

    fn new() -> Self {
        Self { mix_us: [0; Self::LEN], late_us: [0; Self::LEN], n: 0 }
    }

    fn add(&mut self, mix: std::time::Duration, late_ns: u64) {
        self.mix_us[self.n] = u32::try_from(mix.as_micros()).unwrap_or(u32::MAX);
        self.late_us[self.n] = u32::try_from(late_ns / 1000).unwrap_or(u32::MAX);
        self.n += 1;
        if self.n == Self::LEN {
            self.n = 0;
            let (mix, late) = (Self::stats(&mut self.mix_us), Self::stats(&mut self.late_us));
            // Said at warn when mixing nears the real-time budget or the wake is late (a demotion
            // shows as the wake error growing back to milliseconds), else only at debug: a normal
            // stream does not log every 5.5 s for hours.
            macro_rules! say {
                ($level:ident) => {
                    tracing::$level!(
                        mix_avg_us = mix.0,
                        mix_p99_us = mix.1,
                        mix_max_us = mix.2,
                        late_p50_us = late.3,
                        late_p99_us = late.1,
                        late_max_us = late.2,
                        "audio thread timing over {} blocks",
                        Self::LEN
                    )
                };
            }
            if Self::anomaly(mix.1, late.1) {
                say!(warn);
            } else {
                say!(debug);
            }
        }
    }

    /// Whether a window's p99 mixing time is over half the real-time budget, or its p99 wake error over
    /// a millisecond.
    fn anomaly(mix_p99_us: u32, late_p99_us: u32) -> bool {
        u64::from(mix_p99_us) * 1000 > crate::engine::AUDIO_COMPUTATION_NS / 2 || late_p99_us > 1000
    }

    /// (average, p99, max, median) of the window; sorts it.
    fn stats(a: &mut [u32; Self::LEN]) -> (u32, u32, u32, u32) {
        let avg = (a.iter().map(|&x| u64::from(x)).sum::<u64>() / Self::LEN as u64) as u32;
        a.sort_unstable();
        (avg, a[Self::LEN * 99 / 100], a[Self::LEN - 1], a[Self::LEN / 2])
    }
}

/// Pure tick state machine for the dedicated audio thread, parameterized over [`AudioClock`] so
/// production uses [`SystemClock`] and tests drive [`crate::rt::SimClock`] deterministically.
pub struct AudioRunner<C: AudioClock> {
    clock: C,
    rate: u32,
    step_ns: u64,
    next_tick_ns: u64,
    mixer: Mixer,
    tones: crate::rt::PreMap<ToneState>,
    rings: crate::rt::PreMap<AudioConsumer>,
    levels: crate::rt::PreMap<Arc<AtomicLevels>>,
    pending_removals: crate::rt::PreMap<u64>,
    heard: Arc<[u64]>,
    master_taps: Vec<Weak<Mutex<VecDeque<f32>>>>,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    monitor: Option<Feeder>,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    monitor_epoch: u64,
    cmd_rx: mpsc::Receiver<AudioCmd>,
    recycle_rx: mpsc::Receiver<Block>,
    bridge_tx: Option<mpsc::SyncSender<Block>>,
    afx_tx: Option<mpsc::SyncSender<AfxJob>>,
    needs_plugin: fn(AudioEffect, u32) -> bool,
    next_build_id: u64,
    ring_scratch: Vec<f32>,
    tone_scratch: Vec<f32>,
    blocks_scratch: Vec<Block>,
}

impl<C: AudioClock> std::fmt::Debug for AudioRunner<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioRunner")
            .field("rate", &self.rate)
            .field("step_ns", &self.step_ns)
            .field("next_tick_ns", &self.next_tick_ns)
            .field("sources", &self.levels.len())
            .finish_non_exhaustive()
    }
}

impl<C: AudioClock> AudioRunner<C> {
    /// Creates an `AudioRunner` backed by `clock`, mixing at `rate` Hz with initial `latency_ns`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        clock: C,
        rate: u32,
        latency_ns: u64,
        cmd_rx: mpsc::Receiver<AudioCmd>,
        recycle_rx: mpsc::Receiver<Block>,
        bridge_tx: Option<mpsc::SyncSender<Block>>,
        afx_tx: Option<mpsc::SyncSender<AfxJob>>,
        needs_plugin: fn(AudioEffect, u32) -> bool,
    ) -> Self {
        let rate = rate.max(1);
        let step_ns = BLOCK as u64 * 1_000_000_000 / u64::from(rate);
        let next_tick_ns = clock.now_ns();
        Self {
            clock,
            rate,
            step_ns,
            next_tick_ns,
            mixer: {
                let mut m = Mixer::new(rate, latency_ns);
                // The background thread makes the resamplers other rates need.
                m.set_background_resamplers(afx_tx.is_some());
                m
            },
            tones: crate::rt::PreMap::new(),
            rings: crate::rt::PreMap::new(),
            levels: crate::rt::PreMap::new(),
            pending_removals: crate::rt::PreMap::new(),
            heard: Arc::from([]),
            master_taps: Vec::new(),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            monitor: None,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            monitor_epoch: 0,
            cmd_rx,
            recycle_rx,
            bridge_tx,
            afx_tx,
            needs_plugin,
            next_build_id: 1,
            ring_scratch: Vec::with_capacity(INITIAL_RING_SCRATCH_SAMPLES),
            tone_scratch: Vec::with_capacity(BLOCK * 2),
            blocks_scratch: Vec::with_capacity(8),
        }
    }

    /// Executes one audio cadence step without sleeping.
    pub fn step(&mut self) {
        while let Ok(b) = self.recycle_rx.try_recv() {
            self.mixer.recycle(b);
        }
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            self.handle_cmd(cmd);
        }
        self.tidy_after_commands();

        let now = self.clock.now_ns();
        self.expire_scheduled_removals(now);
        self.push_tones(now);
        self.drain_rings();

        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if let Some(f) = &self.monitor {
            let epoch = f.device_epoch();
            if epoch != self.monitor_epoch {
                self.monitor_epoch = epoch;
                self.mixer.set_monitor_device(f.device_id());
            }
        }

        self.blocks_scratch.clear();
        self.mixer.mix_until_into(now, &mut self.blocks_scratch);

        if !self.blocks_scratch.is_empty() {
            for (id, atom) in &self.levels {
                if let Some(lvl) = self.mixer.levels(*id) {
                    atom.store(&lvl);
                }
                if let Some(fast) = self.mixer.levels_over(*id, crate::mix::METER_FAST_BLOCKS) {
                    atom.store_fast(&fast);
                }
            }
            if !self.master_taps.is_empty() {
                self.master_taps.retain(|w| w.strong_count() > 0);
                for w in &self.master_taps {
                    if let Some(buf) = w.upgrade() {
                        for b in &self.blocks_scratch {
                            push_tap_interleaved(&buf, &b.tracks[0], self.rate);
                        }
                    }
                }
            }
        }

        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if let Some(f) = &self.monitor {
            if self.mixer.has_monitored() {
                for b in &self.blocks_scratch {
                    f.push(&b.monitor);
                }
            } else {
                f.idle();
            }
        }

        for b in self.blocks_scratch.drain(..) {
            if let Some(tx) = &self.bridge_tx {
                match tx.try_send(b) {
                    Ok(()) => {}
                    Err(mpsc::TrySendError::Full(b) | mpsc::TrySendError::Disconnected(b)) => {
                        self.mixer.recycle(b);
                    }
                }
            } else {
                self.mixer.recycle(b);
            }
        }

        let after = self.clock.now_ns();
        if after.saturating_sub(self.next_tick_ns) > MAX_TICK_CATCH_UP_NS {
            self.next_tick_ns = after + self.step_ns;
        } else {
            while self.next_tick_ns <= after {
                self.next_tick_ns += self.step_ns;
            }
        }
    }

    /// Executes one step and sleeps on the injected clock until the next audio tick target.
    pub fn tick(&mut self) {
        self.step();
        self.clock.sleep_until_ns(self.next_tick_ns);
    }

    /// Runs ticks continuously until `stop` becomes true. Every [`TimingWindow::LEN`] blocks it says
    /// how long mixing took and how late the thread woke (one log line, no allocation to keep it:
    /// two arrays of that length), which is how the real-time class's `computation` is sized and
    /// a demotion (the wake error growing back to milliseconds) is seen.
    pub fn run(&mut self, stop: &AtomicBool) {
        let mut window = TimingWindow::new();
        while !stop.load(Ordering::Acquire) {
            let began = std::time::Instant::now();
            self.step();
            let mix = began.elapsed();
            if stop.load(Ordering::Acquire) {
                break;
            }
            let due = self.next_tick_ns;
            self.clock.sleep_until_ns(due);
            window.add(mix, self.clock.now_ns().saturating_sub(due));
        }
    }

    fn handle_cmd(&mut self, cmd: AudioCmd) {
        match cmd {
            AudioCmd::Spare(s) => self.mixer.add_spare(s),
            AudioCmd::WantResampler(r) => self.mixer.want_resampler(r),
            AudioCmd::SpareResampler(r) => self.mixer.add_spare_resampler(r),
            AudioCmd::EnsureSource { id, levels } => {
                self.pending_removals.remove(&id);
                self.mixer.ensure(id);
                if let Some(old) = self.levels.insert(id, levels) {
                    self.drop_elsewhere(AfxJob::DropLevels(old));
                }
            }
            AudioCmd::RemoveSource(id) => self.remove_source_now(id),
            AudioCmd::ScheduleRemove { id, after_ns } => {
                self.pending_removals.insert(id, after_ns);
            }
            AudioCmd::AttachRing { id, consumer } => {
                self.pending_removals.remove(&id);
                self.mixer.ensure(id);
                if let Some(old) = self.rings.insert(id, consumer) {
                    self.drop_elsewhere(AfxJob::DropRing(old));
                }
            }
            AudioCmd::FlushSource(id) => {
                if let Some(c) = self.rings.get_mut(&id) {
                    c.clear();
                }
                self.mixer.flush_source(id);
            }
            AudioCmd::SetTone { id, tone, levels } => {
                self.pending_removals.remove(&id);
                match tone {
                    Some(spec) => {
                        self.mixer.ensure(id);
                        if let Some(l) = levels
                            && let Some(old) = self.levels.insert(id, l)
                        {
                            self.drop_elsewhere(AfxJob::DropLevels(old));
                        }
                        self.tones.insert(id, ToneState { spec, pushed_to: None });
                    }
                    None => {
                        self.tones.remove(&id);
                    }
                }
            }
            AudioCmd::SetSettings { id, settings, levels } => {
                self.pending_removals.remove(&id);
                self.mixer.set(id, settings);
                if let Some(old) = self.levels.insert(id, levels) {
                    self.drop_elsewhere(AfxJob::DropLevels(old));
                }
            }
            AudioCmd::PushKey { id, talk, down } => self.mixer.push_key(id, talk, down),
            AudioCmd::SetOutputCapture { id, capture } => {
                self.mixer.set_output_capture(id, capture);
            }
            AudioCmd::SetEffects { id, effects, levels } => {
                self.pending_removals.remove(&id);
                if let Some(l) = levels
                    && let Some(old) = self.levels.insert(id, l)
                {
                    self.drop_elsewhere(AfxJob::DropLevels(old));
                }
                let next_id = &mut self.next_build_id;
                let mut gen_id = || {
                    let cur = *next_id;
                    *next_id += 1;
                    cur
                };
                let (evicted, missing) =
                    self.mixer.set_effects_nonblocking(id, &effects, &mut gen_id, &self.needs_plugin);
                if !evicted.is_empty() {
                    if let Some(afx) = &self.afx_tx {
                        let _ = afx.try_send(AfxJob::DropFilters(evicted));
                    } else {
                        drop(evicted);
                    }
                }
                if !missing.is_empty()
                    && let Some(afx) = &self.afx_tx
                {
                    let _ = afx.try_send(AfxJob::Build { id, rate: self.rate, items: missing });
                }
            }
            AudioCmd::AttachPlugins { id, built } => {
                let out = self.mixer.attach_plugins(id, built);
                if !out.rejected.is_empty() {
                    if let Some(afx) = &self.afx_tx {
                        let _ = afx.try_send(AfxJob::DropPlugins(out.rejected));
                    } else {
                        drop(out.rejected);
                    }
                }
                if !out.rebuild.is_empty()
                    && let Some(afx) = &self.afx_tx
                {
                    let _ = afx.try_send(AfxJob::Build { id, rate: self.rate, items: out.rebuild });
                }
            }
            AudioCmd::SetHeard(heard) => {
                self.mixer.sync_active_sorted(&heard);
                // The list it had is freed on the background thread, not here (a scene switch
                // must not free memory on the real-time thread).
                let old = std::mem::replace(&mut self.heard, heard);
                if let Some(afx) = &self.afx_tx {
                    let _ = afx.try_send(AfxJob::DropHeard(old));
                }
            }
            AudioCmd::SetLowLatency(on) => self.mixer.set_low_latency(on),
            AudioCmd::SetTruePeak(on) => self.mixer.set_true_peak(on),
            AudioCmd::SetDucking(ducking) => self.mixer.set_ducking(ducking),
            AudioCmd::SetLoudness(target) => self.mixer.set_loudness(target),
            AudioCmd::AttachTap { id, tap } => self.mixer.attach_tap(id, tap),
            AudioCmd::AttachMasterTap(tap) => {
                self.master_taps.retain(|w| w.strong_count() > 0);
                self.master_taps.push(tap);
            }
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            AudioCmd::SetMonitor { feeder, dev_id, epoch } => {
                self.monitor_epoch = epoch;
                self.mixer.set_monitor_device(dev_id);
                if let Some(old) = std::mem::replace(&mut self.monitor, feeder) {
                    if let Some(afx) = &self.afx_tx {
                        let _ = afx.try_send(AfxJob::DropFeeder(old));
                    } else {
                        drop(old);
                    }
                }
            }
            #[cfg(test)]
            AudioCmd::QuerySettings { id, reply } => {
                let _ = reply.send(self.mixer.settings(id));
            }
        }
    }

    fn remove_source_now(&mut self, id: u64) {
        self.pending_removals.remove(&id);
        self.tones.remove(&id);
        if let Some(ring) = self.rings.remove(&id) {
            self.drop_elsewhere(AfxJob::DropRing(ring));
        }
        if let Some(l) = self.levels.remove(&id) {
            self.drop_elsewhere(AfxJob::DropLevels(l));
        }
        if let Some(evicted) = self.mixer.remove(id)
            && !evicted.is_empty()
        {
            if let Some(afx) = &self.afx_tx {
                let _ = afx.try_send(AfxJob::DropFilters(evicted));
            } else {
                drop(evicted);
            }
        }
    }

    /// Frees what a job holds on the background thread (here only when there is none).
    fn drop_elsewhere(&self, job: AfxJob) {
        if let Some(afx) = &self.afx_tx {
            let _ = afx.try_send(job);
        }
    }

    /// After commands: the entries of removed sources go to be dropped elsewhere, and spares
    /// are asked for to replace the ones sources took.
    fn tidy_after_commands(&mut self) {
        while let Some(src) = self.mixer.pop_retired() {
            self.drop_elsewhere(AfxJob::DropSource(src));
        }
        while let Some(rs) = self.mixer.pop_retired_resampler() {
            self.drop_elsewhere(AfxJob::DropResampler(rs));
        }
        self.mixer.release_held();
        let out_rate = self.mixer.resampler_out_rate();
        while let Some(in_rate) = self.mixer.pop_resampler_request() {
            self.drop_elsewhere(AfxJob::MakeResampler { in_rate, out_rate });
        }
        let n = self.mixer.take_spare_request();
        if n > 0 {
            self.drop_elsewhere(AfxJob::MakeSpare { rate: self.rate, n });
        }
    }

    fn expire_scheduled_removals(&mut self, now: u64) {
        if self.pending_removals.is_empty() {
            return;
        }
        let lat = self.mixer.latency_ns();
        // One at a time (no list made here): each removal changes the map.
        loop {
            let due =
                self.pending_removals.iter().find_map(|(&id, &after)| (now >= after.saturating_add(lat)).then_some(id));
            let Some(id) = due else { break };
            self.remove_source_now(id);
        }
    }

    fn push_tones(&mut self, now: u64) {
        let rate = self.rate;
        let tau = std::f32::consts::TAU;
        for (&id, tone) in &mut self.tones {
            let every = tone.spec.every_ns.max(1);
            let fresh = now - now % every;
            let mut at = tone.pushed_to.filter(|p| *p + MAX_CATCH_UP_NS >= now).unwrap_or(fresh);
            let heard = self.heard.binary_search(&id).is_ok();
            while at < now + 500_000_000 {
                if heard {
                    let n = (u128::from(tone.spec.beep_ns) * u128::from(rate) / 1_000_000_000) as usize;
                    if n > 0 {
                        self.tone_scratch.clear();
                        self.tone_scratch.reserve(n * 2);
                        let step = tone.spec.hz * tau / rate as f32;
                        for i in 0..n {
                            let v = (i as f32 * step).sin() * 0.5;
                            self.tone_scratch.push(v);
                            self.tone_scratch.push(v);
                        }
                        self.mixer.push(id, &self.tone_scratch, 2, rate, at);
                    }
                }
                at += every;
            }
            tone.pushed_to = Some(at);
        }
    }

    fn drain_rings(&mut self) {
        for (&id, consumer) in &mut self.rings {
            let heard = self.heard.binary_search(&id).is_ok() || self.pending_removals.contains_key(&id);
            while let Some(piece) = consumer.peek() {
                let needed = piece.samples();
                if self.ring_scratch.len() < needed {
                    self.ring_scratch.resize(needed, 0.0);
                }
                match consumer.pop_into(&mut self.ring_scratch) {
                    Ok(p) => {
                        if heard {
                            self.mixer.push(
                                id,
                                &self.ring_scratch[..p.samples()],
                                usize::from(p.channels),
                                p.rate,
                                p.ts_ns,
                            );
                        }
                    }
                    Err(PopError::TooSmall { needed }) => {
                        self.ring_scratch.resize(needed, 0.0);
                    }
                    Err(PopError::Empty) => break,
                }
            }
        }
    }

    #[cfg(test)]
    fn blocks_allocated(&self) -> usize {
        self.mixer.blocks_allocated()
    }
}

/// Owned handle to the running `ferrocast-audio` and `ferrocast-afx-bg` threads.
pub struct AudioHandle {
    cmd_tx: mpsc::Sender<AudioCmd>,
    recycle_tx: mpsc::SyncSender<Block>,
    stop: Arc<AtomicBool>,
    audio_thread: Option<JoinHandle<()>>,
    afx_thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for AudioHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioHandle").field("stopped", &self.stop.load(Ordering::Relaxed)).finish_non_exhaustive()
    }
}

impl AudioHandle {
    /// Spawns `ferrocast-afx-bg` and `ferrocast-audio` (`AudioRunner<SystemClock>`), returning the
    /// handle alongside the temporary Commit 3 bridge receiver for mixed blocks.
    pub fn spawn(rate: u32, latency_ns: u64) -> Result<(Self, mpsc::Receiver<Block>), crate::AvError> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCmd>();
        let (recycle_tx, recycle_rx) = mpsc::sync_channel::<Block>(RECYCLE_CAP);
        let (bridge_tx, bridge_rx) = mpsc::sync_channel::<Block>(BRIDGE_CAP);
        let (afx_tx, afx_rx) = mpsc::sync_channel::<AfxJob>(AFX_QUEUE);

        let afx_cmd = cmd_tx.clone();
        let afx_thread = std::thread::Builder::new()
            .name("ferrocast-afx-bg".into())
            .spawn(move || run_afx_bg(afx_rx, afx_cmd, AudioFilter::build_plugin))
            .map_err(|e| crate::AvError::Source(format!("audio plugin worker didn't start ({e})")))?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_audio = stop.clone();
        let audio_thread = match std::thread::Builder::new().name("ferrocast-audio".into()).spawn(move || {
            let _priority_guard = crate::engine::raise_audio_thread();
            let mut runner = AudioRunner::new(
                SystemClock,
                rate,
                latency_ns,
                cmd_rx,
                recycle_rx,
                Some(bridge_tx),
                Some(afx_tx),
                AudioFilter::effect_needs_plugin,
            );
            runner.run(&stop_audio);
        }) {
            Ok(t) => t,
            Err(e) => {
                let _ = afx_thread.join();
                return Err(crate::AvError::Source(format!("the audio thread didn't start ({e})")));
            }
        };

        Ok((
            Self { cmd_tx, recycle_tx, stop, audio_thread: Some(audio_thread), afx_thread: Some(afx_thread) },
            bridge_rx,
        ))
    }

    /// Sends a command to `ferrocast-audio` without blocking.
    pub fn send(&self, cmd: AudioCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    /// Clones a non-blocking handle for returning mixed `Block`s to the mixer's pool.
    #[must_use]
    pub fn recycle_sender(&self) -> mpsc::SyncSender<Block> {
        self.recycle_tx.clone()
    }

    /// Clones the command sender for background workers (`open_soon`, `watchdog`, `media-open`).
    #[must_use]
    pub fn cmd_sender(&self) -> mpsc::Sender<AudioCmd> {
        self.cmd_tx.clone()
    }

    /// Stops and joins `ferrocast-audio` and then `ferrocast-afx-bg` cleanly.
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.audio_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.afx_thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for AudioHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mix::Monitoring;
    use crate::rt::{SimClock, SpscAudioRing};
    use std::time::{Duration, Instant};

    const RATE: u32 = 48_000;
    const STEP_NS: u64 = BLOCK as u64 * 1_000_000_000 / RATE as u64;

    /// A normal stream's windows (mixing 0.2 ms, a late wake of 0.7 ms at worst in the soak) stay at
    /// debug; mixing over half the 4 ms budget, or a wake over 1 ms at p99, is said at warn.
    #[test]
    fn the_timing_line_is_a_warning_only_for_an_anomaly() {
        assert!(!TimingWindow::anomaly(350, 700), "the soak's normal windows");
        assert!(!TimingWindow::anomaly(2000, 1000), "the bounds are not over");
        assert!(TimingWindow::anomaly(2100, 10), "mixing over half the budget");
        assert!(TimingWindow::anomaly(200, 1001), "a wake over a millisecond late");
        assert!(TimingWindow::anomaly(1481, 3110), "the soak's one bad window");
    }

    /// Gate: a 250 ms video stall leaves `AudioRunner<SimClock>` cadence steady (exact 21.33 ms
    /// blocks, 0 monitor underruns, 0 dropped blocks) and `AtomicLevels::load()` non-blocking
    /// even while a simulated `Engine::state` lock is held across the entire stall.
    #[test]
    fn simclock_250ms_video_stall_keeps_audio_cadence_steady_and_levels_nonblocking() {
        let t0 = 10_000_000_000u64;
        let clock = SimClock::at(t0);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (recycle_tx, recycle_rx) = mpsc::sync_channel(RECYCLE_CAP);
        let (bridge_tx, bridge_rx) = mpsc::sync_channel(BRIDGE_CAP);

        let mut runner = AudioRunner::new(
            clock.clone(),
            RATE,
            40_000_000,
            cmd_rx,
            recycle_rx,
            Some(bridge_tx),
            None,
            AudioFilter::effect_needs_plugin,
        );

        let levels = AtomicLevels::shared();
        let (mut prod, cons) = SpscAudioRing::for_seconds(3, RATE, 2, 480);
        cmd_tx.send(AudioCmd::EnsureSource { id: 1, levels: levels.clone() }).unwrap_or_else(|e| panic!("{e}"));
        cmd_tx
            .send(AudioCmd::SetSettings {
                id: 1,
                settings: SourceSettings { monitoring: Monitoring::AndOutput, ..Default::default() },
                levels: levels.clone(),
            })
            .unwrap_or_else(|e| panic!("{e}"));
        cmd_tx.send(AudioCmd::AttachRing { id: 1, consumer: cons }).unwrap_or_else(|e| panic!("{e}"));
        cmd_tx.send(AudioCmd::SetHeard(Arc::from([1]))).unwrap_or_else(|e| panic!("{e}"));

        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let mon = crate::monitor::TestMonitor::new(RATE, RATE, 2);
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        cmd_tx
            .send(AudioCmd::SetMonitor { feeder: Some(mon.feeder.clone()), dev_id: Some("test".into()), epoch: 1 })
            .unwrap_or_else(|e| panic!("{e}"));

        let mut mon_out = [0.0f32; BLOCK * 2];
        let mut pull_monitor = |_tick: usize| {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            if mon.queued_frames() >= BLOCK + 1920 {
                mon.pull(&mut mon_out);
            }
            let _ = &mut mon_out;
        };

        let mut push_piece = |at_ns: u64, phase: &mut usize| {
            let mut pcm = [0.0f32; 960 * 2];
            for (i, f) in pcm.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                let v = ((*phase + i) as f32 * 440.0 * std::f32::consts::TAU / RATE as f32).sin() * 0.5;
                f[0] = v;
                f[1] = v;
            }
            *phase += 960;
            prod.push(&pcm, 2, RATE, at_ns).unwrap_or_else(|e| panic!("{e:?}"));
        };

        let mut phase = 0usize;
        let mut feed_up_to = |now_ns: u64, next_piece: &mut u64| {
            while *next_piece <= now_ns {
                push_piece(*next_piece, &mut phase);
                *next_piece += 20_000_000;
            }
        };
        let mut next_piece = t0;
        let mut seen_ts = Vec::new();

        // Warm-up: 20 ticks (~426 ms) with video draining bridge every 2 ticks.
        for k in 0..20 {
            feed_up_to(clock.now_ns(), &mut next_piece);
            runner.tick();
            pull_monitor(k);
            if k % 2 == 1 {
                while let Ok(b) = bridge_rx.try_recv() {
                    seen_ts.push(b.ts_ns);
                    let _ = recycle_tx.try_send(b);
                }
            }
        }
        assert!(runner.blocks_allocated() <= 8, "pool settled: {}", runner.blocks_allocated());

        // Simulated 250 ms video stall: another thread holds the `State` mutex for the whole stall
        // and never touches `bridge_rx`.
        let fake_state_lock = Arc::new(Mutex::new(()));
        let held_guard = fake_state_lock.lock().unwrap_or_else(|e| e.into_inner());
        let stall_start = clock.now_ns();
        let before_stall_count = seen_ts.len();

        let mut stall_blocks_read_from_levels = Vec::new();
        for k in 20..32 {
            // 12 audio ticks = 256 ms (> 250 ms) while video is completely wedged.
            feed_up_to(clock.now_ns(), &mut next_piece);
            runner.tick();
            pull_monitor(k);
            // `levels()` reads `AtomicLevels` directly without touching `fake_state_lock` or `AudioRunner`.
            let t_read = Instant::now();
            let lv = levels.load();
            assert!(t_read.elapsed() < Duration::from_millis(5), "levels() must be wait-free");
            stall_blocks_read_from_levels.push(lv.peak[0]);
        }
        assert!(clock.now_ns() - stall_start >= 250_000_000, "simulated > 250 ms stall");
        assert!(
            stall_blocks_read_from_levels.iter().all(|p| *p > -12.0),
            "levels stayed active during video stall: {stall_blocks_read_from_levels:?}"
        );
        drop(held_guard);

        // Video resumes and drains all blocks produced during the 250 ms stall.
        while let Ok(b) = bridge_rx.try_recv() {
            seen_ts.push(b.ts_ns);
            let _ = recycle_tx.try_send(b);
        }
        let stall_blocks = seen_ts.len() - before_stall_count;
        assert_eq!(stall_blocks, 12, "all 12 audio blocks queued during 250 ms stall without drop");
        assert!(
            seen_ts.windows(2).all(|w| w[1] - w[0] == STEP_NS),
            "steady 1024-frame cadence across stall: {seen_ts:?}"
        );

        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            let (short, played) = mon.stats();
            assert_eq!(short, 0, "0 monitor underruns during 250 ms video stall");
            assert!(played > 0, "monitor kept playing through video stall");
        }
    }

    /// Gate: a 500 ms slow plugin constructor on `ferrocast-afx-bg` never stalls `AudioRunner`
    /// ticks; audio ticks complete in microseconds and attach the plugin once construction ends.
    #[test]
    fn slow_plugin_ctor_does_not_stall_audio_ticks() {
        #[derive(Debug)]
        struct SlowGainPlugin;
        impl SoundPlugin for SlowGainPlugin {
            fn process(&mut self, ch: [&mut [f32]; 2]) -> bool {
                let [l, r] = ch;
                for s in l.iter_mut().chain(r.iter_mut()) {
                    *s *= 0.25;
                }
                true
            }
        }

        fn slow_builder(effect: AudioEffect, _rate: u32) -> Option<Box<dyn SoundPlugin>> {
            if matches!(effect, AudioEffect::Plugin { slot: 77 }) {
                std::thread::sleep(Duration::from_millis(500));
                Some(Box::new(SlowGainPlugin))
            } else {
                None
            }
        }
        fn test_needs_plugin(effect: AudioEffect, _rate: u32) -> bool {
            matches!(effect, AudioEffect::Plugin { slot: 77 })
        }

        let t0 = 5_000_000_000u64;
        let clock = SimClock::at(t0);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (_recycle_tx, recycle_rx) = mpsc::sync_channel(RECYCLE_CAP);
        let (bridge_tx, bridge_rx) = mpsc::sync_channel(BRIDGE_CAP);
        let (afx_tx, afx_rx) = mpsc::sync_channel(AFX_QUEUE);

        let afx_cmd = cmd_tx.clone();
        let afx_thread = std::thread::Builder::new()
            .name("test-afx-bg".into())
            .spawn(move || run_afx_bg(afx_rx, afx_cmd, slow_builder))
            .unwrap_or_else(|e| panic!("{e}"));

        let mut runner = AudioRunner::new(
            clock.clone(),
            RATE,
            0,
            cmd_rx,
            recycle_rx,
            Some(bridge_tx),
            Some(afx_tx),
            test_needs_plugin,
        );

        let levels = AtomicLevels::shared();
        let (mut prod, cons) = SpscAudioRing::for_seconds(2, RATE, 2, 1024);
        cmd_tx.send(AudioCmd::EnsureSource { id: 1, levels: levels.clone() }).unwrap_or_else(|e| panic!("{e}"));
        cmd_tx.send(AudioCmd::AttachRing { id: 1, consumer: cons }).unwrap_or_else(|e| panic!("{e}"));
        cmd_tx.send(AudioCmd::SetHeard(Arc::from([1]))).unwrap_or_else(|e| panic!("{e}"));
        cmd_tx
            .send(AudioCmd::SetEffects { id: 1, effects: vec![AudioEffect::Plugin { slot: 77 }], levels: None })
            .unwrap_or_else(|e| panic!("{e}"));

        // Immediately tick 10 times while the 500 ms ctor runs off-thread: every tick must stay < 10 ms.
        let mut worst_tick = Duration::ZERO;
        let mut t = t0;
        for _ in 0..10 {
            prod.push(&[0.8f32; BLOCK * 2], 2, RATE, t).unwrap_or_else(|e| panic!("{e:?}"));
            let s = Instant::now();
            runner.tick();
            worst_tick = worst_tick.max(s.elapsed());
            t += STEP_NS;
        }
        assert!(worst_tick < Duration::from_millis(50), "audio tick stalled on plugin ctor: {worst_tick:?}");
        // While building, dry audio passed through (~0.8).
        let mut initial_samples = Vec::new();
        while let Ok(b) = bridge_rx.try_recv() {
            initial_samples.extend(b.tracks[0].iter().copied());
        }
        assert!(initial_samples.iter().any(|v| (*v - 0.8).abs() < 1e-4), "passthrough while building");

        // Wait for the 500 ms background ctor to finish and deliver AttachPlugins.
        std::thread::sleep(Duration::from_millis(600));
        prod.push(&[0.8f32; BLOCK * 2], 2, RATE, t).unwrap_or_else(|e| panic!("{e:?}"));
        runner.tick();
        t += STEP_NS;
        prod.push(&[0.8f32; BLOCK * 2], 2, RATE, t).unwrap_or_else(|e| panic!("{e:?}"));
        runner.tick();

        let mut last = None;
        while let Ok(b) = bridge_rx.try_recv() {
            last = Some(b);
        }
        let b = last.unwrap_or_else(|| panic!("expected mixed block after plugin attach"));
        assert!((b.tracks[0][0] - 0.2).abs() < 1e-4, "attached SlowGainPlugin scaled 0.8 -> 0.2: {}", b.tracks[0][0]);

        drop(runner);
        drop(cmd_tx);
        afx_thread.join().unwrap_or_else(|_| panic!("afx thread panicked"));
    }

    /// Reordering or dragging filter sliders while an off-thread plugin build is in flight keeps
    /// the build matched by `build_id` and retunes/attaches cleanly without spawning redundant builds.
    #[test]
    fn slider_drag_and_reorder_during_plugin_build_attaches_and_retunes() {
        use std::sync::atomic::AtomicU32;
        static BUILDS: AtomicU32 = AtomicU32::new(0);

        #[derive(Debug)]
        struct RetunableDenoise(u32);
        impl SoundPlugin for RetunableDenoise {
            fn process(&mut self, ch: [&mut [f32]; 2]) -> bool {
                let scale = self.0 as f32 / 100.0;
                let [l, r] = ch;
                for s in l.iter_mut().chain(r.iter_mut()) {
                    *s *= scale;
                }
                true
            }
            fn retune(&mut self, effect: &AudioEffect) -> bool {
                if let AudioEffect::Denoise { strength } = effect {
                    self.0 = *strength;
                    true
                } else {
                    false
                }
            }
        }

        fn build_denoise(effect: AudioEffect, _rate: u32) -> Option<Box<dyn SoundPlugin>> {
            if let AudioEffect::Denoise { strength } = effect {
                BUILDS.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(80));
                Some(Box::new(RetunableDenoise(strength)))
            } else {
                None
            }
        }
        fn needs(effect: AudioEffect, rate: u32) -> bool {
            rate == 48_000 && matches!(effect, AudioEffect::Denoise { .. })
        }

        BUILDS.store(0, Ordering::SeqCst);
        let t0 = 2_000_000_000u64;
        let clock = SimClock::at(t0);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (_recycle_tx, recycle_rx) = mpsc::sync_channel(RECYCLE_CAP);
        let (bridge_tx, bridge_rx) = mpsc::sync_channel(BRIDGE_CAP);
        let (afx_tx, afx_rx) = mpsc::sync_channel(AFX_QUEUE);
        let afx_cmd = cmd_tx.clone();
        let afx_thread = std::thread::Builder::new()
            .name("test-afx-retune".into())
            .spawn(move || run_afx_bg(afx_rx, afx_cmd, build_denoise))
            .unwrap_or_else(|e| panic!("{e}"));

        let mut runner = AudioRunner::new(clock, RATE, 0, cmd_rx, recycle_rx, Some(bridge_tx), Some(afx_tx), needs);
        let levels = AtomicLevels::shared();
        let (mut prod, cons) = SpscAudioRing::for_seconds(2, RATE, 2, 1024);
        cmd_tx.send(AudioCmd::EnsureSource { id: 1, levels }).unwrap_or_else(|e| panic!("{e}"));
        cmd_tx.send(AudioCmd::AttachRing { id: 1, consumer: cons }).unwrap_or_else(|e| panic!("{e}"));
        cmd_tx.send(AudioCmd::SetHeard(Arc::from([1]))).unwrap_or_else(|e| panic!("{e}"));
        cmd_tx
            .send(AudioCmd::SetEffects { id: 1, effects: vec![AudioEffect::Denoise { strength: 50 }], levels: None })
            .unwrap_or_else(|e| panic!("{e}"));
        runner.tick();
        // Drag slider to 25 and reorder behind a Gain filter before the 80 ms build finishes.
        cmd_tx
            .send(AudioCmd::SetEffects {
                id: 1,
                effects: vec![AudioEffect::Gain { db: 0.0 }, AudioEffect::Denoise { strength: 25 }],
                levels: None,
            })
            .unwrap_or_else(|e| panic!("{e}"));
        runner.tick();

        std::thread::sleep(Duration::from_millis(160));
        prod.push(&[0.8f32; BLOCK * 2], 2, RATE, t0 + STEP_NS * 2).unwrap_or_else(|e| panic!("{e:?}"));
        runner.tick();
        prod.push(&[0.8f32; BLOCK * 2], 2, RATE, t0 + STEP_NS * 3).unwrap_or_else(|e| panic!("{e:?}"));
        runner.tick();

        assert_eq!(BUILDS.load(Ordering::SeqCst), 1, "slider drag + reorder reused the in-flight build");
        let mut last = None;
        while let Ok(b) = bridge_rx.try_recv() {
            last = Some(b);
        }
        let b = last.unwrap_or_else(|| panic!("expected block"));
        assert!((b.tracks[0][0] - 0.2).abs() < 1e-4, "retuned to strength 25 (0.8 * 0.25 = 0.2): {}", b.tracks[0][0]);

        drop(runner);
        drop(cmd_tx);
        afx_thread.join().unwrap_or_else(|_| panic!("afx thread panicked"));
    }

    /// Master tap (`AttachMasterTap`) and source tap (`AttachTap`) both receive audio directly
    /// from `AudioRunner`.
    #[test]
    fn master_and_source_taps_receive_audio_from_audio_runner() {
        let t0 = 1_000_000_000u64;
        let clock = SimClock::at(t0);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (_recycle_tx, recycle_rx) = mpsc::sync_channel(RECYCLE_CAP);
        let mut runner =
            AudioRunner::new(clock, RATE, 0, cmd_rx, recycle_rx, None, None, AudioFilter::effect_needs_plugin);

        let levels = AtomicLevels::shared();
        let (mut prod, cons) = SpscAudioRing::for_seconds(1, RATE, 2, 1024);
        let src_tap_buf = Arc::new(Mutex::new(VecDeque::new()));
        let master_tap_buf = Arc::new(Mutex::new(VecDeque::new()));

        cmd_tx.send(AudioCmd::EnsureSource { id: 1, levels }).unwrap_or_else(|e| panic!("{e}"));
        cmd_tx.send(AudioCmd::AttachRing { id: 1, consumer: cons }).unwrap_or_else(|e| panic!("{e}"));
        cmd_tx.send(AudioCmd::SetHeard(Arc::from([1]))).unwrap_or_else(|e| panic!("{e}"));
        cmd_tx.send(AudioCmd::AttachTap { id: 1, tap: Arc::downgrade(&src_tap_buf) }).unwrap_or_else(|e| panic!("{e}"));
        cmd_tx.send(AudioCmd::AttachMasterTap(Arc::downgrade(&master_tap_buf))).unwrap_or_else(|e| panic!("{e}"));

        prod.push(&[0.5f32; BLOCK * 2], 2, RATE, t0).unwrap_or_else(|e| panic!("{e:?}"));
        runner.tick();
        runner.tick();

        let src_samples: Vec<f32> = src_tap_buf.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
        let master_samples: Vec<f32> = master_tap_buf.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
        assert_eq!(src_samples.len(), BLOCK);
        assert!(src_samples.iter().all(|v| (*v - 0.5).abs() < 1e-6));
        assert!(master_samples.len() >= BLOCK);
        assert!(master_samples.iter().any(|v| (*v - 0.5).abs() < 1e-6));
    }

    /// `FlushSource` empties both the SPSC ring and the mixer's queued sound immediately.
    #[test]
    fn flush_source_clears_ring_and_mixer_queue() {
        let t0 = 1_000_000_000u64;
        let clock = SimClock::at(t0);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (_recycle_tx, recycle_rx) = mpsc::sync_channel(RECYCLE_CAP);
        let (bridge_tx, bridge_rx) = mpsc::sync_channel(BRIDGE_CAP);
        let mut runner = AudioRunner::new(
            clock,
            RATE,
            0,
            cmd_rx,
            recycle_rx,
            Some(bridge_tx),
            None,
            AudioFilter::effect_needs_plugin,
        );

        let levels = AtomicLevels::shared();
        let (mut prod, cons) = SpscAudioRing::for_seconds(1, RATE, 2, 1024);
        cmd_tx.send(AudioCmd::EnsureSource { id: 1, levels }).unwrap_or_else(|e| panic!("{e}"));
        cmd_tx.send(AudioCmd::AttachRing { id: 1, consumer: cons }).unwrap_or_else(|e| panic!("{e}"));
        cmd_tx.send(AudioCmd::SetHeard(Arc::from([1]))).unwrap_or_else(|e| panic!("{e}"));

        prod.push(&[0.9f32; BLOCK * 4], 2, RATE, t0).unwrap_or_else(|e| panic!("{e:?}"));
        runner.tick();
        while bridge_rx.try_recv().is_ok() {}
        cmd_tx.send(AudioCmd::FlushSource(1)).unwrap_or_else(|e| panic!("{e}"));
        runner.tick();

        let b = bridge_rx.try_recv().unwrap_or_else(|e| panic!("{e}"));
        assert!(b.tracks[0].iter().all(|v| *v == 0.0), "flushed source emitted silence");
    }

    /// Sidechain compression runs on `ferrocast-audio` via `AudioCmd::SetEffects`: source 2's
    /// signal keys source 1's compressor in topological order, and `AudioCmd::RemoveSource(2)`
    /// cleanly falls back to a silent key so source 1 recovers without crashing.
    #[test]
    fn sidechain_ducks_music_under_voice_and_recovers_on_remove() {
        let t0 = 4_000_000_000u64;
        let clock = SimClock::at(t0);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (recycle_tx, recycle_rx) = mpsc::sync_channel(RECYCLE_CAP);
        let (bridge_tx, bridge_rx) = mpsc::sync_channel(BRIDGE_CAP);
        let mut runner = AudioRunner::new(
            clock,
            RATE,
            0,
            cmd_rx,
            recycle_rx,
            Some(bridge_tx),
            None,
            AudioFilter::effect_needs_plugin,
        );

        let music_levels = AtomicLevels::shared();
        let voice_levels = AtomicLevels::shared();
        let (mut music_prod, music_cons) = SpscAudioRing::for_seconds(2, RATE, 2, 1024);
        let (mut voice_prod, voice_cons) = SpscAudioRing::for_seconds(2, RATE, 2, 1024);

        // Music (id 1) goes to track 1; Voice (id 2) goes to track 2 (so track 1 isolates Music).
        cmd_tx
            .send(AudioCmd::SetSettings {
                id: 1,
                settings: SourceSettings { tracks: 0b01, ..Default::default() },
                levels: music_levels,
            })
            .unwrap_or_else(|e| panic!("{e}"));
        cmd_tx
            .send(AudioCmd::SetSettings {
                id: 2,
                settings: SourceSettings { tracks: 0b10, ..Default::default() },
                levels: voice_levels,
            })
            .unwrap_or_else(|e| panic!("{e}"));
        cmd_tx.send(AudioCmd::AttachRing { id: 1, consumer: music_cons }).unwrap_or_else(|e| panic!("{e}"));
        cmd_tx.send(AudioCmd::AttachRing { id: 2, consumer: voice_cons }).unwrap_or_else(|e| panic!("{e}"));
        cmd_tx.send(AudioCmd::SetHeard(Arc::from([1, 2]))).unwrap_or_else(|e| panic!("{e}"));
        cmd_tx
            .send(AudioCmd::SetEffects {
                id: 1,
                effects: vec![AudioEffect::Compressor {
                    ratio: 10.0,
                    threshold_db: -30.0,
                    attack_ms: 5.0,
                    release_ms: 20.0,
                    output_db: 0.0,
                    sidechain: Some(2),
                }],
                levels: None,
            })
            .unwrap_or_else(|e| panic!("{e}"));

        let music_pcm = [0.5f32; BLOCK * 2];
        let voice_pcm = [0.5f32; BLOCK * 2];
        let mut t = t0;
        let mut last_ducked = 0.0f32;
        for _ in 0..8 {
            music_prod.push(&music_pcm, 2, RATE, t).unwrap_or_else(|e| panic!("{e:?}"));
            voice_prod.push(&voice_pcm, 2, RATE, t).unwrap_or_else(|e| panic!("{e:?}"));
            runner.tick();
            while let Ok(b) = bridge_rx.try_recv() {
                last_ducked = b.tracks[0][BLOCK - 1].abs();
                let _ = recycle_tx.try_send(b);
            }
            t += STEP_NS;
        }
        assert!(last_ducked < 0.08, "music must duck deeply while voice keys it, got {last_ducked}");

        // Removing the key source switches to silence immediately; music recovers cleanly.
        cmd_tx.send(AudioCmd::RemoveSource(2)).unwrap_or_else(|e| panic!("{e}"));
        cmd_tx.send(AudioCmd::SetHeard(Arc::from([1]))).unwrap_or_else(|e| panic!("{e}"));
        let mut last_recovered = 0.0f32;
        for _ in 0..10 {
            music_prod.push(&music_pcm, 2, RATE, t).unwrap_or_else(|e| panic!("{e:?}"));
            runner.tick();
            while let Ok(b) = bridge_rx.try_recv() {
                last_recovered = b.tracks[0][BLOCK - 1].abs();
                let _ = recycle_tx.try_send(b);
            }
            t += STEP_NS;
        }
        assert!(
            (last_recovered - 0.5).abs() < 0.03,
            "music must recover to full level after key removal, got {last_recovered}"
        );
    }
}
