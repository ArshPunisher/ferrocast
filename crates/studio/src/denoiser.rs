//! Strong noise removal: DeepFilterNet 3 (MIT/Apache-2.0, github.com/Rikorose/DeepFilterNet) on
//! the CPU through tract, its model built in. It takes out what RNNoise leaves (keyboards, fans,
//! a busy room) and keeps the voice natural. It works on 10 ms blocks at 48 kHz and hands each
//! back one block later (10 ms, plus the model's own look ahead). Built only with the
//! `deepfilter` feature (the release builds), so day-to-day builds don't compile the model.

use std::collections::VecDeque;

use df::tract::{DfParams, DfTract, RuntimeParams};
use ndarray15::Array2;

/// What strength 100 gives: DeepFilterNet's own "no limit" (100 dB and up) zeroes the soft ends of
/// words (a -180 dB cliff at 100), so the top of the scale is held to this (M2-01).
const MAX_ATTEN_DB: f32 = 35.0;

/// How much quieter noise may get at a strength (1 to 100): 0.6 dB a step up to 50 (30 dB), as
/// it always was, so every saved strength up to 50 means what it meant; above that 0.1 dB a
/// step up to [`MAX_ATTEN_DB`], so a saved 100 (which used to mean "no limit") is the strongest
/// removal that still leaves a voice whole, with no jump between 99 and 100.
fn atten_db(strength: u32) -> f32 {
    let s = strength.clamp(1, 100);
    // Worked in whole numbers (50 is exactly 30 dB).
    let db = if s <= 50 { (s * 6) as f32 / 10.0 } else { 30.0 + (s - 50) as f32 / 10.0 };
    db.min(MAX_ATTEN_DB)
}

/// An input whose every sample stays at or under this (about -90 dBFS) is digital silence: a
/// device that is switched off or muted in hardware, not a quiet room (that is never this low).
const SILENT_PEAK: f32 = 3.16e-5;
/// Seconds of digital silence the model has already worked through before it stops being run on
/// more of it. Its own state (the level it measures the sound against) adapts to silence over
/// a few seconds: stopped after one it is still a third of what the voice before it left, and
/// the voice after the silence is judged differently from by the model that ran all along; after
/// three it is within a dB or two.
const SILENT_SECS: usize = 3;

/// Where the knee starts (-1.9 dBFS: speech under it is untouched) and the ceiling it reaches
/// (-0.09 dBFS, under the 0.9999 the model complains at).
const KNEE: f32 = 0.8;
const CEILING: f32 = 0.99;

/// What goes into the model: the sample itself up to the knee, a smooth tanh knee from there that
/// tops out at the ceiling however far over full scale it was (a gain before the noise removal on
/// a loud source). No state, no allocation, no delay; a NaN stays a NaN as it was.
fn soft_limit(x: f32) -> f32 {
    let a = x.abs();
    if a <= KNEE {
        return x;
    }
    (KNEE + (CEILING - KNEE) * ((a - KNEE) / (CEILING - KNEE)).tanh()).copysign(x)
}

/// DeepFilterNet over a stream of planar samples: blocks in, cleaned blocks out, one block late.
pub(crate) struct Denoiser {
    df: DfTract,
    hop: usize,
    inp: Vec<VecDeque<f32>>,
    out: Vec<VecDeque<f32>>,
    noisy: Array2<f32>,
    clean: Array2<f32>,
    strength: u32,
    /// Blocks in a row (all channels) that were digital silence.
    quiet_blocks: usize,
    /// Blocks passed through without the model (digital silence after the first second of it).
    skipped_blocks: u64,
    /// Off in a test that compares with the model run on everything.
    skip_silence: bool,
}

impl Denoiser {
    /// For `channels` channels at 48 kHz.
    pub(crate) fn new(channels: usize, strength: u32) -> Result<Denoiser, String> {
        let channels = channels.clamp(1, 8);
        let rp = RuntimeParams::default_with_ch(channels).with_atten_lim(atten_db(strength));
        let df = DfTract::new(DfParams::default(), &rp).map_err(|e| format!("{e:#}"))?;
        let hop = df.hop_size;
        // One block of silence first: what's handed back is always there.
        let cap = hop * 4 + 1024;
        let out = (0..channels)
            .map(|_| {
                let mut q = VecDeque::with_capacity(cap);
                q.extend(std::iter::repeat_n(0.0, hop));
                q
            })
            .collect();
        Ok(Denoiser {
            df,
            hop,
            inp: (0..channels).map(|_| VecDeque::with_capacity(cap)).collect(),
            out,
            noisy: Array2::zeros((channels, hop)),
            clean: Array2::zeros((channels, hop)),
            strength,
            quiet_blocks: 0,
            skipped_blocks: 0,
            skip_silence: true,
        })
    }

    /// How late the cleaned sound comes out, in samples: the block of silence it starts with,
    /// the rest of the model's window, and the frames it looks ahead (40 ms with its model).
    pub(crate) fn latency(&self) -> usize {
        self.df.fft_size + self.df.lookahead * self.hop
    }

    pub(crate) fn channels(&self) -> usize {
        self.inp.len()
    }

    /// Samples never came between what it has taken in and what comes next: the half frame it
    /// holds from before is let go, not joined to the sound after the hole (the frames after
    /// would also have been misaligned until the next one).
    pub(crate) fn gap(&mut self) {
        for q in &mut self.inp {
            q.clear();
        }
        // The stretch of silence it was in is over: the sound after the hole is the model's.
        self.quiet_blocks = 0;
    }

    pub(crate) fn set_strength(&mut self, strength: u32) {
        if strength != self.strength {
            self.df.set_atten_lim(atten_db(strength));
            self.strength = strength;
        }
    }

    /// Cleans the samples in place (every plane as long as the first; at most `channels`).
    pub(crate) fn run(&mut self, planes: &mut [&mut [f32]]) {
        let n = planes.first().map_or(0, |p| p.len());
        let ch = self.channels().min(planes.len());
        for (c, p) in planes.iter().enumerate().take(ch) {
            self.inp[c].extend(p.iter().copied());
        }
        // A channel the stream lacks is silence.
        for c in ch..self.channels() {
            self.inp[c].extend(std::iter::repeat_n(0.0, n));
        }
        while self.inp[0].len() >= self.hop {
            for c in 0..self.channels() {
                for (i, s) in self.inp[c].drain(..self.hop).enumerate() {
                    self.noisy[[c, i]] = soft_limit(s);
                }
            }
            // Digital silence (a muted or switched-off device) is given back as silence once the
            // model has had a few seconds of it: the model's state and its held delay are
            // silence by then, so the sound that follows comes out in the same place, and the
            // first block with anything in it goes to the model with nothing to wait for.
            let silent = self.noisy.iter().all(|s| s.abs() <= SILENT_PEAK);
            self.quiet_blocks = if silent { self.quiet_blocks + 1 } else { 0 };
            if self.skip_silence && silent && self.quiet_blocks > SILENT_SECS * 48_000 / self.hop {
                // Exactly nothing: not the input's last bits of dither, which the model would have
                // taken down and which would come in as a step here.
                self.clean.fill(0.0);
                self.skipped_blocks += 1;
            } else if self.df.process(self.noisy.view(), self.clean.view_mut()).is_err() {
                // As it was rather than nothing.
                self.clean.assign(&self.noisy);
            }
            for c in 0..self.channels() {
                self.out[c].extend(self.clean.row(c).iter().copied());
            }
        }
        for (c, p) in planes.iter_mut().enumerate().take(ch) {
            for s in p.iter_mut() {
                *s = self.out[c].pop_front().unwrap_or(0.0);
            }
        }
        for c in ch..self.channels() {
            let k = n.min(self.out[c].len());
            self.out[c].drain(..k);
        }
    }
}

impl std::fmt::Debug for Denoiser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Denoiser").field("channels", &self.channels()).field("strength", &self.strength).finish()
    }
}

/// Worker-side denoiser interface so tests can inject slow model loads or init failures
/// deterministically without loading ONNX weights.
trait WorkerEngine {
    fn latency(&self) -> usize;
    fn set_strength(&mut self, strength: u32);
    fn run(&mut self, planes: &mut [&mut [f32]]);
    /// A block (or more) before the next one never came (the inbox dropped it): what the model
    /// holds of the sound before it must not be joined to what comes now.
    fn gap(&mut self) {}
}

impl WorkerEngine for Denoiser {
    fn latency(&self) -> usize {
        Denoiser::latency(self)
    }

    fn set_strength(&mut self, strength: u32) {
        Denoiser::set_strength(self, strength);
    }

    fn run(&mut self, planes: &mut [&mut [f32]]) {
        Denoiser::run(self, planes);
    }

    fn gap(&mut self) {
        Denoiser::gap(self);
    }
}

/// The built-in model's own delay (samples): `Denoiser::latency` once it's made, known before
/// (a test holds them equal), so the mixer files the sound the same way from the first block.
const MODEL_LATENCY: usize = 1920;
/// How far behind the model's answers are taken: two mixer blocks. The mixer never waits for
/// the model (it ran inside the frame loop and held it up to 100 ms when the computer was busy,
/// 18 Nov); the frame loop mixes two blocks back to back now and then (30 fps), so one block
/// behind left the second's answer not back yet a third of the time, and it gave up.
const PIPE: usize = 2048;
/// Bounded worker inbox (3 blocks ≈ 64 ms): blocks are never queued while the model is still
/// loading, and a transient CPU spike drops the oldest queued block instead of accumulating an
/// unbounded backlog that permanently trips `gave_up` (M2-02).
const MAX_INBOX: usize = 3;
const MAX_OUTBOX: usize = 8;
const MAX_RECYCLE: usize = 12;
/// Samples a change to the sound as it came is eased over when there's no answer to fade from.
const DECLICK: usize = 480;
/// After the model's answer was late, the sound stays dry at least this long (six blocks) before
/// it goes back to the model's: answers late every few blocks flipped it between the two at
/// block rate (a tear: the noise coming and going) and a stretch in the model's hands that short
/// was not worth the change.
const DRY_HOLD: usize = 6144;
/// The longest the model may take to get ready before the light noise removal is used instead.
const READY_WITHIN: std::time::Duration = std::time::Duration::from_secs(10);

/// How long a stretch is judged over, and the share of it that may pass dry (the model's answer
/// late) before the light noise removal takes over: a computer just too slow for it flips
/// between the two every few blocks, and the noise pumped all stream long.
const JUDGE_SECS: usize = 10;
const MOST_DRY: f32 = 0.2;

/// One stereo audio block moving between `Plugin` and its worker thread, tagged with the exact
/// stream sample index `at` of its first input sample so startup skip and drop-oldest recovery
/// keep exact sample alignment.
struct BlockBuf {
    at: u64,
    l: Vec<f32>,
    r: Vec<f32>,
}

struct Queue {
    inbox: VecDeque<BlockBuf>,
    outbox: VecDeque<BlockBuf>,
    /// Recycled stereo buffers so steady-state `process()` performs zero heap allocations.
    recycle: Vec<(Vec<f32>, Vec<f32>)>,
    pending_strength: Option<u32>,
    ready: bool,
    dead: bool,
    closed: bool,
}

impl Queue {
    fn new() -> Self {
        let mut recycle = Vec::with_capacity(MAX_RECYCLE);
        for _ in 0..6 {
            recycle.push((Vec::with_capacity(1024), Vec::with_capacity(1024)));
        }
        Self {
            inbox: VecDeque::with_capacity(MAX_INBOX),
            outbox: VecDeque::with_capacity(MAX_OUTBOX),
            recycle,
            pending_strength: None,
            ready: false,
            dead: false,
            closed: false,
        }
    }

    fn take_buf(&mut self, n: usize) -> (Vec<f32>, Vec<f32>) {
        self.recycle.pop().unwrap_or_else(|| (Vec::with_capacity(n), Vec::with_capacity(n)))
    }

    fn put_buf(&mut self, mut l: Vec<f32>, mut r: Vec<f32>) {
        if self.recycle.len() < MAX_RECYCLE {
            l.clear();
            r.clear();
            self.recycle.push((l, r));
        }
    }
}

struct Shared {
    q: std::sync::Mutex<Queue>,
    wake: std::sync::Condvar,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared").finish_non_exhaustive()
    }
}

/// Our engine's sound filter: a stereo Denoiser on a thread of its own (the model can't move
/// between threads, the mixer's filters do). The model is made on that thread (it takes a moment)
/// without enqueuing blocks until it is ready, and every subsequent block goes through a bounded
/// drop-oldest queue. Until the model is ready, or when an answer is late, the sound plays as it
/// came, delayed the same, so nothing is silenced and nothing moves in time; switching between
/// the two fades over a block. Never blocks in `process()`.
#[derive(Debug)]
struct Plugin {
    shared: std::sync::Arc<Shared>,
    /// The model's answers not played yet (planar), and the stream sample index of `wet[0][0]`.
    wet: [VecDeque<f32>; 2],
    wet_at: u64,
    /// Earliest stream sample index from which `wet` holds valid (post-warmup) model output.
    wet_valid_from: u64,
    /// True once at least one block has been drained from `outbox`.
    have_wet_stream: bool,
    /// True once the model has actually delivered at least one usable wet block (judging starts
    /// only after this, and failed init before this leaves zero handover delay).
    ever_wet: bool,
    /// Samples handed out so far.
    out_at: u64,
    /// What came in, `latency` samples ago (what plays when there's no answer).
    dry: [VecDeque<f32>; 2],
    /// Pre-allocated scratch for the current block's dry samples (zero per-block allocation).
    dry_scratch: [Vec<f32>; 2],
    /// The last block was the model's.
    was_wet: bool,
    /// Samples left to stay dry after a late answer (see `DRY_HOLD`).
    dry_hold: usize,
    /// Samples judged in this stretch, and how many of them passed dry, since the first wet block.
    judged: usize,
    dry_run: usize,
    /// Too slow for this computer or failed init: the next block isn't taken.
    gave_up: bool,
    /// True if init failed or timed out before ever delivering wet audio (reports 0 latency and
    /// no handover buffer so `light_denoise` is not stuck behind an 82.7 ms delay line).
    failed_init: bool,
    /// The last sample handed out, each side.
    last: [f32; 2],
    /// When it was made, and how long startup may take before giving up.
    made: std::time::Instant,
    ready_within: std::time::Duration,
    strength: u32,
}

impl Drop for Plugin {
    fn drop(&mut self) {
        let mut q = self.shared.q.lock().unwrap_or_else(|e| e.into_inner());
        q.closed = true;
        self.shared.wake.notify_all();
    }
}

impl Plugin {
    fn start(strength: u32) -> Result<Self, String> {
        Self::start_with(strength, READY_WITHIN, |s| Denoiser::new(2, s))
    }

    fn start_with<E, F>(strength: u32, ready_within: std::time::Duration, make: F) -> Result<Self, String>
    where
        E: WorkerEngine,
        F: FnOnce(u32) -> Result<E, String> + Send + 'static,
    {
        let shared =
            std::sync::Arc::new(Shared { q: std::sync::Mutex::new(Queue::new()), wake: std::sync::Condvar::new() });
        let worker_shared = shared.clone();
        std::thread::Builder::new()
            .name("ferrocast-denoise".into())
            .spawn(move || {
                // As soon as the frame loop: it waits for nothing here any more, but a model
                // woken late leaves the sound dry.
                ferrocore::engine::raise_this_thread();
                let mut d = match make(strength) {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(error = %e, "the strong noise removal couldn't start");
                        let mut q = worker_shared.q.lock().unwrap_or_else(|e| e.into_inner());
                        q.dead = true;
                        return;
                    }
                };
                if d.latency() != MODEL_LATENCY {
                    tracing::warn!(latency = d.latency(), "the noise removal's model has another delay");
                }
                {
                    let mut q = worker_shared.q.lock().unwrap_or_else(|e| e.into_inner());
                    q.ready = true;
                    if q.closed {
                        return;
                    }
                }
                // Where the next block should start, to tell when blocks were dropped before it.
                let mut next_at: Option<u64> = None;
                loop {
                    let (new_strength, blk) = {
                        let mut q = worker_shared.q.lock().unwrap_or_else(|e| e.into_inner());
                        while !q.closed && q.inbox.is_empty() && q.pending_strength.is_none() {
                            q = worker_shared.wake.wait(q).unwrap_or_else(|e| e.into_inner());
                        }
                        if q.closed {
                            return;
                        }
                        (q.pending_strength.take(), q.inbox.pop_front())
                    };
                    if let Some(s) = new_strength {
                        d.set_strength(s);
                    }
                    if let Some(mut blk) = blk {
                        if next_at.is_some_and(|e| e != blk.at) {
                            d.gap();
                        }
                        next_at = Some(blk.at + blk.l.len() as u64);
                        d.run(&mut [&mut blk.l, &mut blk.r]);
                        let mut q = worker_shared.q.lock().unwrap_or_else(|e| e.into_inner());
                        if q.closed {
                            return;
                        }
                        if q.outbox.len() >= MAX_OUTBOX
                            && let Some(old) = q.outbox.pop_front()
                        {
                            q.put_buf(old.l, old.r);
                        }
                        q.outbox.push_back(blk);
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let silence = || {
            let mut q = VecDeque::with_capacity(MODEL_LATENCY + PIPE + 1024);
            q.extend(std::iter::repeat_n(0.0, MODEL_LATENCY + PIPE));
            q
        };
        Ok(Self {
            shared,
            wet: [VecDeque::with_capacity(PIPE + 2048), VecDeque::with_capacity(PIPE + 2048)],
            wet_at: 0,
            wet_valid_from: 0,
            have_wet_stream: false,
            ever_wet: false,
            out_at: 0,
            dry: [silence(), silence()],
            dry_scratch: [Vec::with_capacity(1024), Vec::with_capacity(1024)],
            was_wet: false,
            dry_hold: 0,
            judged: 0,
            dry_run: 0,
            gave_up: false,
            failed_init: false,
            last: [0.0; 2],
            made: std::time::Instant::now(),
            ready_within,
            strength,
        })
    }

    /// Takes finished blocks from `q.outbox`, keeping exact stream alignment across startup skips
    /// and drop-oldest discontinuities, and recycles every block's buffers back into `q.recycle`.
    fn drain_outbox(
        q: &mut Queue,
        wet: &mut [VecDeque<f32>; 2],
        wet_at: &mut u64,
        wet_valid_from: &mut u64,
        have_wet_stream: &mut bool,
    ) {
        while let Some(blk) = q.outbox.pop_front() {
            let expected = *wet_at + wet[0].len() as u64;
            if !*have_wet_stream || blk.at != expected {
                // First block after model load, or a block was dropped during a CPU spike:
                // reset the wet stream to `blk.at` and hold dry until the model's `MODEL_LATENCY`
                // overlap-add warmup flushes out, avoiding a silence dip or phase jump.
                wet[0].clear();
                wet[1].clear();
                *wet_at = blk.at;
                *wet_valid_from = blk.at + MODEL_LATENCY as u64;
                *have_wet_stream = true;
            }
            wet[0].extend(blk.l.iter().copied());
            wet[1].extend(blk.r.iter().copied());
            q.put_buf(blk.l, blk.r);
        }
    }

    /// True when the model's post-warmup answers for `from..from + n` (with `from = out_at - PIPE`)
    /// are completely available at the front of `self.wet`.
    fn wet_for(&mut self, n: usize) -> bool {
        if !self.have_wet_stream || self.out_at < PIPE as u64 {
            return false;
        }
        let from = self.out_at - PIPE as u64;
        // Answers for samples already played: let go.
        let old = from.saturating_sub(self.wet_at).min(self.wet[0].len() as u64) as usize;
        self.wet[0].drain(..old);
        self.wet[1].drain(..old);
        self.wet_at += old as u64;
        from >= self.wet_valid_from && self.wet_at == from && self.wet[0].len() >= n
    }
}

impl ferrocore::afx::SoundPlugin for Plugin {
    fn process(&mut self, ch: [&mut [f32]; 2]) -> bool {
        let [l, r] = ch;
        let n = l.len().min(r.len());
        if self.gave_up {
            return false;
        }
        // Single short lock: check worker state, enqueue only once ready (drop-oldest if full),
        // and drain any completed wet blocks without ever sleeping inside the mixer lock.
        let ready = {
            let mut q = self.shared.q.lock().unwrap_or_else(|e| e.into_inner());
            if q.dead {
                self.failed_init = !self.ever_wet;
                self.gave_up = true;
                return false;
            }
            let ready = q.ready;
            if ready {
                let (mut bl, mut br) = q.take_buf(n);
                bl.extend_from_slice(&l[..n]);
                br.extend_from_slice(&r[..n]);
                while q.inbox.len() >= MAX_INBOX {
                    if let Some(old) = q.inbox.pop_front() {
                        q.put_buf(old.l, old.r);
                    }
                }
                q.inbox.push_back(BlockBuf { at: self.out_at, l: bl, r: br });
                self.shared.wake.notify_one();
            }
            Self::drain_outbox(
                &mut q,
                &mut self.wet,
                &mut self.wet_at,
                &mut self.wet_valid_from,
                &mut self.have_wet_stream,
            );
            ready
        };
        if !self.ever_wet && self.made.elapsed() > self.ready_within {
            tracing::warn!("the strong noise removal didn't get ready: the light one takes over");
            self.failed_init = true;
            self.gave_up = true;
            return false;
        }
        // The sound as it came, delayed as the model's is (reuses pre-allocated scratch).
        for (c, x) in [&*l, &*r].into_iter().enumerate() {
            self.dry[c].extend(&x[..n]);
            self.dry_scratch[c].clear();
            self.dry_scratch[c].extend(self.dry[c].drain(..n));
        }
        let mut wet = ready && self.wet_for(n);
        if wet {
            self.ever_wet = true;
        }
        // The model's answers for this block are here (`have`), and they're played (`wet`).
        let have = wet;
        // A late answer holds the sound dry for a while; one that is here again meanwhile is let go.
        if self.ever_wet {
            if !have {
                self.dry_hold = DRY_HOLD;
            } else if self.dry_hold > 0 {
                self.dry_hold = self.dry_hold.saturating_sub(n);
                wet = false;
            }
        }
        // Judging starts only after the first wet block arrives (never penalized during slow
        // startup or pipeline fill). One that gives up fades to dry here so `light_denoise`
        // starts from there with no step.
        if self.ever_wet {
            self.judged += n;
            if !wet {
                self.dry_run += n;
            }
            if self.judged >= JUDGE_SECS * 48_000 {
                self.gave_up = self.dry_run as f32 > self.judged as f32 * MOST_DRY;
                if self.gave_up {
                    tracing::warn!(
                        dry_pct = self.dry_run * 100 / self.judged,
                        "the strong noise removal is too slow for this computer"
                    );
                    wet = false;
                }
                self.judged = 0;
                self.dry_run = 0;
            }
        }
        for (c, x) in [&mut *l, &mut *r].into_iter().enumerate() {
            // Where the last block ended, for a change with no answer to fade from.
            let step = self.last[c] - self.dry_scratch[c].first().copied().unwrap_or(0.0);
            for (i, v) in x[..n].iter_mut().enumerate() {
                let w = if have { self.wet[c].get(i).copied().unwrap_or(0.0) } else { 0.0 };
                let d = self.dry_scratch[c][i];
                let t = i as f32 / n as f32;
                *v = match (self.was_wet, wet, have) {
                    // A change between the two fades across the block: no step, no click.
                    (false, true, _) => d + (w - d) * t,
                    (true, false, true) => w + (d - w) * t,
                    // The answer late: the sound as it came, eased in from where the last block
                    // ended (fading from no answer at all dipped to silence; CodeRabbit).
                    (true, false, false) => d + step * (1.0 - (i as f32 / DECLICK as f32)).max(0.0),
                    (true, true, _) => w,
                    (false, false, _) => d,
                };
            }
            if let Some(end) = x[..n].last() {
                self.last[c] = *end;
            }
        }
        self.out_at += n as u64;
        self.was_wet = wet;
        true
    }

    fn latency_frames(&self) -> usize {
        if self.failed_init {
            return 0;
        }
        let dead = self.shared.q.lock().map(|q| q.dead).unwrap_or(false);
        if dead && !self.ever_wet { 0 } else { MODEL_LATENCY + PIPE }
    }

    fn hand_over(&mut self) -> Option<[Vec<f32>; 2]> {
        if self.failed_init || !self.ever_wet {
            return None;
        }
        Some([self.dry[0].drain(..).collect(), self.dry[1].drain(..).collect()])
    }

    fn retune(&mut self, effect: &ferrocore::afx::AudioEffect) -> bool {
        let ferrocore::afx::AudioEffect::Denoise { strength } = *effect else { return false };
        let mut q = self.shared.q.lock().unwrap_or_else(|e| e.into_inner());
        if q.dead || self.gave_up {
            return false;
        }
        if strength != self.strength {
            self.strength = strength;
            q.pending_strength = Some(strength.clamp(1, 100));
            self.shared.wake.notify_one();
        }
        true
    }
}

/// Gives our engine the strong noise removal (once is enough).
pub(crate) fn install() {
    ferrocore::afx::set_denoiser(|strength| match Plugin::start(strength) {
        Ok(p) => Some(Box::new(p)),
        Err(e) => {
            tracing::warn!(error = %e, "the strong noise removal couldn't start");
            None
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A steady hiss (a fan), the same every run.
    fn hiss(n: usize, seed: &mut u32) -> Vec<f32> {
        (0..n)
            .map(|_| {
                *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((*seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.2
            })
            .collect()
    }

    fn rms(s: &[f32]) -> f32 {
        (s.iter().map(|x| x * x).sum::<f32>() / s.len().max(1) as f32).sqrt()
    }

    fn db_to_mul(db: f32) -> f32 {
        10f32.powf(db / 20.0)
    }

    fn mul_to_db(m: f32) -> f32 {
        20.0 * m.max(1e-9).log10()
    }

    fn pink_noise(n: usize, target_rms: f32, seed: &mut u32) -> Vec<f32> {
        let (mut b0, mut b1, mut b2, mut b3, mut b4, mut b5, mut b6) = (0.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let w = ((*seed >> 8) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0;
            b0 = 0.99886 * b0 + w * 0.0555179;
            b1 = 0.99332 * b1 + w * 0.0750759;
            b2 = 0.969_00 * b2 + w * 0.153_852;
            b3 = 0.86650 * b3 + w * 0.3104856;
            b4 = 0.55000 * b4 + w * 0.5329522;
            b5 = -0.7616 * b5 - w * 0.0168980;
            let p = b0 + b1 + b2 + b3 + b4 + b5 + b6 + w * 0.5362;
            b6 = w * 0.115926;
            out.push(p);
        }
        let r = rms(&out).max(1e-9);
        for v in &mut out {
            *v *= target_rms / r;
        }
        out
    }

    fn bandpass_voice(x: &[f32]) -> Vec<f32> {
        // 2nd-order Butterworth HP 300 Hz + LP 3400 Hz at 48 kHz.
        let biquad = |inp: &[f32], b: [f64; 3], a: [f64; 2]| -> Vec<f32> {
            let (mut z0, mut z1) = (0.0f64, 0.0f64);
            inp.iter()
                .map(|&s| {
                    let s = f64::from(s);
                    let y = b[0] * s + z0;
                    z0 = b[1] * s - a[0] * y + z1;
                    z1 = b[2] * s - a[1] * y;
                    y as f32
                })
                .collect()
        };
        let hp = |fc: f64| -> ([f64; 3], [f64; 2]) {
            let k = (std::f64::consts::PI * fc / 48_000.0).tan();
            let q = std::f64::consts::FRAC_1_SQRT_2;
            let a0 = 1.0 + k / q + k * k;
            ([1.0 / a0, -2.0 / a0, 1.0 / a0], [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0])
        };
        let lp = |fc: f64| -> ([f64; 3], [f64; 2]) {
            let k = (std::f64::consts::PI * fc / 48_000.0).tan();
            let q = std::f64::consts::FRAC_1_SQRT_2;
            let a0 = 1.0 + k / q + k * k;
            ([k * k / a0, 2.0 * k * k / a0, k * k / a0], [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0])
        };
        let (hb, ha) = hp(300.0);
        let (lb, la) = lp(3400.0);
        biquad(&biquad(x, hb, ha), lb, la)
    }

    /// Objective speech-clarity test: synthetic voiced harmonic formant speech + pink noise
    /// across strengths. Verifies:
    /// 1) voice-band energy (300–3400 Hz) stays preserved at every strength;
    /// 2) background pink noise reduction increases smoothly and monotonically with strength;
    /// 3) no attenuation cliff between strength 99 and 100 (capped smoothly at 30 dB);
    /// 4) no level pumping across continuous speech.
    #[test]
    fn speech_clarity_and_noise_reduction_across_strengths() {
        let r = 48_000usize;
        let make_articulated = |n: usize, target_rms: f32| -> Vec<f32> {
            let mut out = Vec::with_capacity(n);
            let mut phase = 0.0f64;
            let rate = 48_000.0f64;
            for i in 0..n {
                let t = i as f64 / rate;
                let f0 = 140.0
                    + 25.0 * (std::f64::consts::TAU * 0.8 * t).sin()
                    + 10.0 * (std::f64::consts::TAU * 2.1 * t).cos();
                phase += std::f64::consts::TAU * f0 / rate;
                let f1 = 520.0 + 220.0 * (std::f64::consts::TAU * 1.9 * t).sin();
                let f2 = 1550.0 + 550.0 * (std::f64::consts::TAU * 1.4 * t).cos();
                let f3 = 2600.0 + 300.0 * (std::f64::consts::TAU * 1.1 * t).sin();
                let mut s = 0.0f64;
                for k in 1..=26 {
                    let fk = f0 * f64::from(k);
                    let g1 = 1.0 / ((fk - f1).powi(2) + 55.0f64.powi(2)).sqrt();
                    let g2 = 0.7 / ((fk - f2).powi(2) + 80.0f64.powi(2)).sqrt();
                    let g3 = 0.35 / ((fk - f3).powi(2) + 110.0f64.powi(2)).sqrt();
                    let env = (g1 + g2 + g3) / f64::from(k).powf(0.5);
                    let phi = std::f64::consts::PI * f64::from(k * k) / 26.0;
                    s += (phase * f64::from(k) + phi).sin() * env;
                }
                let cyc = (t * 3.2) % 1.0;
                let env_t = if cyc < 0.78 { (std::f64::consts::PI * cyc / 0.78).sin().powf(0.6) } else { 0.0 };
                out.push((s * env_t) as f32);
            }
            let cur = rms(&out).max(1e-9);
            for v in &mut out {
                *v *= target_rms / cur;
            }
            out
        };

        let mut seed = 42u32;
        let total = 5 * r; // 1 s pink noise, 2.5 s speech+noise, 1.5 s pink noise
        let noise = pink_noise(total, db_to_mul(-36.0), &mut seed);
        let speech = make_articulated(2 * r + r / 2, db_to_mul(-28.0));
        let mut input = noise.clone();
        for i in 0..speech.len() {
            input[r + i] += speech[i];
        }
        let sp_ref_bp = bandpass_voice(&speech[r / 4..speech.len() - r / 4]);
        let sp_ref_rms = rms(&sp_ref_bp);

        let mut prev_pause_nr = 0.0f32;
        let mut pause_at_99 = 0.0f32;
        let mut pause_at_100 = 0.0f32;
        for strength in [20u32, 40, 65, 80, 99, 100] {
            let Ok(mut d) = Denoiser::new(1, strength) else { panic!("model starts") };
            let lat = d.latency();
            let mut padded = input.clone();
            padded.resize(total + lat + 1024, 0.0);
            let mut out = Vec::with_capacity(padded.len());
            for chunk in padded.chunks(1024) {
                let mut buf = chunk.to_vec();
                d.run(&mut [&mut buf]);
                out.extend_from_slice(&buf);
            }
            let aligned = &out[lat..lat + total];
            let sp_out_bp = bandpass_voice(&aligned[r + r / 4..r + speech.len() - r / 4]);
            let voice_kept_db = mul_to_db(rms(&sp_out_bp) / sp_ref_rms);
            let pause_nr_db = mul_to_db(rms(&aligned[4 * r..5 * r]) / rms(&noise[4 * r..5 * r]));
            // Stability across 0.5 s windows during speech (no pumping).
            let mut w_min = f32::INFINITY;
            let mut w_max = f32::NEG_INFINITY;
            for w in 0..4 {
                let a = r + r / 4 + w * (r / 2);
                let b = a + r / 2;
                let g = mul_to_db(rms(&bandpass_voice(&aligned[a..b])) / rms(&bandpass_voice(&speech[a - r..b - r])));
                w_min = w_min.min(g);
                w_max = w_max.max(g);
            }
            let spread = w_max - w_min;
            assert!(voice_kept_db > -1.5, "s={strength}: voice band dropped {voice_kept_db:+.2} dB");
            assert!(spread < 1.5, "s={strength}: speech pumped {spread:.2} dB across windows");
            assert!(
                pause_nr_db < prev_pause_nr,
                "s={strength}: pause_nr {pause_nr_db:+.2} dB should tighten past {prev_pause_nr:+.2} dB"
            );
            if strength == 99 {
                pause_at_99 = pause_nr_db;
            } else if strength == 100 {
                pause_at_100 = pause_nr_db;
            }
            prev_pause_nr = pause_nr_db;
        }
        assert!(
            (pause_at_100 - pause_at_99).abs() < 2.0,
            "no 99→100 cliff: s=99 gave {pause_at_99:+.2} dB vs s=100 {pause_at_100:+.2} dB"
        );
    }

    /// How late the cleaned sound comes out (in samples), found by matching it to what went in.
    fn delay_of(d: &mut Denoiser) -> usize {
        let mut seed = 3;
        let (mut before, mut after) = (Vec::new(), Vec::new());
        for _ in 0..94 {
            // A rich, voice-like buzz (harmonics of 140 Hz) the lightest touch leaves alone.
            let base = before.len();
            let mut l: Vec<f32> = (0..1024)
                .map(|i| {
                    let t = (base + i) as f32 / 48_000.0;
                    (1..12).map(|k| (std::f32::consts::TAU * 140.0 * k as f32 * t).sin() / k as f32).sum::<f32>() * 0.1
                        + hiss(1, &mut seed)[0] * 0.05
                })
                .collect();
            let mut r = l.clone();
            before.extend_from_slice(&l);
            d.run(&mut [&mut l, &mut r]);
            after.extend_from_slice(&l);
        }
        let (a, b) = (&after[48_000..90_000], &before[..]);
        (0..4_800)
            .max_by(|x, y| {
                let c = |lag: usize| a.iter().enumerate().map(|(i, v)| v * b[48_000 + i - lag]).sum::<f32>();
                c(*x).total_cmp(&c(*y))
            })
            .unwrap_or(0)
    }

    /// The delay it says it has is the one it has (the mixer makes up for exactly that).
    #[test]
    fn its_delay_is_what_it_says() {
        let mut d = Denoiser::new(2, 1).expect("the built-in model starts");
        let said = d.latency();
        assert_eq!(delay_of(&mut d), said, "measured against reported, in samples");
        assert_eq!(said, MODEL_LATENCY, "40 ms with the built-in model (the plugin says it before the model is made)");
    }

    /// Hiss, six seconds of digital silence, then a voice-like buzz, in libobs's 1024-frame
    /// pieces; one run with the silence skipped and one with the model run on all of it.
    fn silence_between(skip: bool) -> (Denoiser, Vec<f32>, Vec<f32>) {
        let mut d = Denoiser::new(2, 100).expect("the built-in model starts");
        d.skip_silence = skip;
        let mut seed = 11;
        let (mut before, mut after) = (Vec::new(), Vec::new());
        for piece in 0..(47 + 282 + 94) {
            let mut l: Vec<f32> = if piece < 47 {
                hiss(1024, &mut seed)
            } else if piece < 47 + 282 {
                vec![0.0; 1024]
            } else {
                let base = before.len();
                (0..1024)
                    .map(|i| {
                        let t = (base + i) as f32 / 48_000.0;
                        (1..12).map(|k| (std::f32::consts::TAU * 140.0 * k as f32 * t).sin() / k as f32).sum::<f32>()
                            * 0.1
                    })
                    .collect()
            };
            let mut r = l.clone();
            before.extend_from_slice(&l);
            d.run(&mut [&mut l, &mut r]);
            after.extend_from_slice(&l);
        }
        (d, before, after)
    }

    /// Digital silence stops costing the model after its first second, and the sound after it
    /// comes out where and as it would have: same place, no click, nothing lost.
    #[test]
    fn digital_silence_is_skipped_after_three_seconds_and_the_voice_after_it_is_the_same() {
        let (all, _, with_model) = silence_between(false);
        let (skipping, before, skipped) = silence_between(true);
        assert_eq!(all.skipped_blocks, 0, "the control never skips");
        // 6 s of silence = 600 blocks of 10 ms; the first 3 s go through the model.
        assert!((295..=305).contains(&skipping.skipped_blocks), "skipped {}", skipping.skipped_blocks);
        assert_eq!(skipped.len(), before.len(), "as many samples out as in");
        // The same sound, in the same place: the first 150 ms of the voice (before the model has
        // made up its mind about a steady buzz, which depends on how long it has sat in silence)
        // come out as they do when the model has run on every block of the silence.
        let onset = (47 + 282) * 1024;
        let first = |v: &[f32]| v[onset - 4_800..onset + 7_200].to_vec();
        let worst = first(&with_model).iter().zip(first(&skipped)).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(worst < 0.03, "the voice's first 150 ms match the model run on everything: {worst}");
        // No click at the edges: no step bigger than the voice itself has.
        let step = |v: &[f32]| v.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(step(&skipped) <= step(&with_model) * 1.05 + 1e-4, "{} vs {}", step(&skipped), step(&with_model));
        // And what the model makes of the buzz afterwards (it takes a steady one for noise) is
        // what it makes of it with nothing skipped.
        let late = |v: &[f32]| rms(&v[onset + 24_000..]);
        assert!(
            late(&skipped) < late(&with_model) * 2.0 && late(&skipped) > late(&with_model) * 0.5,
            "{} vs {}",
            late(&skipped),
            late(&with_model)
        );
        // A very quiet room (not digital silence) is never skipped.
        let mut d = Denoiser::new(2, 100).expect("the built-in model starts");
        let mut seed = 5;
        for _ in 0..200 {
            let mut l: Vec<f32> = hiss(1024, &mut seed).iter().map(|v| v * 1e-3).collect();
            let mut r = l.clone();
            d.run(&mut [&mut l, &mut r]);
        }
        assert_eq!(d.skipped_blocks, 0, "-70 dBFS of room tone still goes through the model");
    }

    /// What a skipped block says is "nothing", exactly: no leftover dither of the input's, which
    /// the model alone would have taken down, not passed on as a step.
    #[test]
    fn skipped_silence_is_exact_zeros() {
        let mut d = Denoiser::new(1, 100).expect("the built-in model starts");
        let mut out = Vec::new();
        for i in 0..(6 * 48_000 / 480) {
            let mut b: Vec<f32> = (0..480).map(|k| if (i + k) % 2 == 0 { 2e-5 } else { -2e-5 }).collect();
            d.run(&mut [&mut b]);
            out.extend_from_slice(&b);
        }
        assert!(d.skipped_blocks > 100, "skipping: {}", d.skipped_blocks);
        assert!(out[out.len() - 48_000..].iter().all(|v| *v == 0.0), "the last second is exact zeros");
    }

    /// A hole in the samples ends the stretch of silence: the model works through the next
    /// seconds again before anything more is skipped.
    #[test]
    fn a_gap_ends_the_silent_stretch() {
        let mut d = Denoiser::new(1, 100).expect("the built-in model starts");
        let silent = |d: &mut Denoiser, blocks: usize| {
            for _ in 0..blocks {
                let mut b = vec![0.0f32; 480];
                d.run(&mut [&mut b]);
            }
        };
        silent(&mut d, 600);
        let skipped = d.skipped_blocks;
        assert!(skipped > 200, "skipping by now: {skipped}");
        d.gap();
        silent(&mut d, 50);
        assert_eq!(d.skipped_blocks, skipped, "after a gap the model runs again");
        silent(&mut d, 400);
        assert!(d.skipped_blocks > skipped, "and the skipping comes back after the silence has gone on");
    }

    /// The model's own state adapts to silence over a few seconds. Skipping too soon leaves it
    /// part way, and a voice after a long silence then comes out differently from what the model
    /// that ran all along gives: not by more than a few dB.
    #[test]
    fn after_a_long_silence_the_voice_is_judged_like_the_model_that_never_skipped() {
        let r = 48_000usize;
        let speech = |n: usize| -> Vec<f32> {
            let mut out = Vec::with_capacity(n);
            let mut phase = 0.0f64;
            for i in 0..n {
                let t = i as f64 / 48_000.0;
                let f0 = 140.0 + 25.0 * (std::f64::consts::TAU * 0.8 * t).sin();
                phase += std::f64::consts::TAU * f0 / 48_000.0;
                let f1 = 520.0 + 220.0 * (std::f64::consts::TAU * 1.9 * t).sin();
                let f2 = 1550.0 + 550.0 * (std::f64::consts::TAU * 1.4 * t).cos();
                let mut sv = 0.0f64;
                for k in 1..=26 {
                    let fk = f0 * f64::from(k);
                    let g1 = 1.0 / ((fk - f1).powi(2) + 55.0f64.powi(2)).sqrt();
                    let g2 = 0.7 / ((fk - f2).powi(2) + 80.0f64.powi(2)).sqrt();
                    let env = (g1 + g2) / f64::from(k).powf(0.5);
                    sv += (phase * f64::from(k) + std::f64::consts::PI * f64::from(k * k) / 26.0).sin() * env;
                }
                let cyc = (t * 3.2) % 1.0;
                let gate = if cyc < 0.78 { (std::f64::consts::PI * cyc / 0.78).sin().powf(0.6) } else { 0.0 };
                out.push((sv * gate) as f32);
            }
            let cur = rms(&out).max(1e-9);
            out.iter().map(|v| v * db_to_mul(-28.0) / cur).collect()
        };
        let mut seed = 42u32;
        let mut burst = || -> Vec<f32> {
            let noise = pink_noise(3 * r, db_to_mul(-36.0), &mut seed);
            noise.iter().zip(speech(3 * r)).map(|(a, b)| a + b).collect()
        };
        let mut input = burst();
        input.extend(std::iter::repeat_n(0.0f32, 6 * r));
        let onset = input.len();
        input.extend(burst());
        let run = |skip: bool| -> Vec<f32> {
            let mut d = Denoiser::new(1, 100).expect("the built-in model starts");
            d.skip_silence = skip;
            let mut out = Vec::new();
            for chunk in input.chunks(1024) {
                let mut b = chunk.to_vec();
                d.run(&mut [&mut b]);
                out.extend_from_slice(&b);
            }
            out
        };
        let (all, skipping) = (run(false), run(true));
        let db = |v: &[f32]| 20.0 * rms(&v[onset..onset + r]).max(1e-9).log10();
        assert!(
            (db(&all) - db(&skipping)).abs() < 3.0,
            "first second of the voice: {:.1} dB vs {:.1} dB",
            db(&all),
            db(&skipping)
        );
    }

    /// DeepFilterNet logs "possible clipping" at 0.9999 and is made for sound inside full scale:
    /// what goes into the model is held under it by a smooth knee, so a +3 dBFS input (a gain
    /// before the noise removal on a loud source) never reaches it, and anything under the knee
    /// goes in bit for bit as it was.
    #[test]
    fn over_range_input_never_reaches_the_model_and_normal_input_is_untouched() {
        let sine = |amp: f32, n: usize| -> Vec<f32> {
            (0..n).map(|i| amp * (std::f32::consts::TAU * 220.0 * i as f32 / 48_000.0).sin()).collect()
        };
        for amp in [db_to_mul(3.0), db_to_mul(12.0), 100.0] {
            let mut d = Denoiser::new(1, 40).expect("the built-in model starts");
            let input = sine(amp, 48_000);
            let mut seen = 0.0f32;
            for chunk in input.chunks(480) {
                let mut b = chunk.to_vec();
                d.run(&mut [&mut b]);
                seen = seen.max(d.noisy.iter().fold(0.0f32, |m, v| m.max(v.abs())));
            }
            assert!(seen > 0.5 && seen < 0.9999, "a {amp:.2} sine reaches the model at {seen}");
        }
        // -20 dBFS and speech peaks to -2 dBFS: not a bit different.
        for amp in [db_to_mul(-20.0), db_to_mul(-6.0), db_to_mul(-2.0)] {
            let mut d = Denoiser::new(1, 40).expect("the built-in model starts");
            let input = sine(amp, 4_800);
            for chunk in input.chunks(480) {
                let mut b = chunk.to_vec();
                d.run(&mut [&mut b]);
                assert!(
                    d.noisy.iter().zip(chunk).all(|(a, b)| a.to_bits() == b.to_bits()),
                    "a {amp:.3} sine goes in unchanged"
                );
            }
        }
        // The knee is smooth: no step, and it only ever goes up as the input does.
        let mut last = (0.0f32, 0.0f32);
        for i in 0..=4000 {
            let x = i as f32 / 1000.0;
            let y = soft_limit(x);
            assert!(y >= last.1 && y - last.1 < 0.003, "smooth and rising at {x}: {} -> {y}", last.1);
            assert_eq!(soft_limit(-x), -y, "odd");
            last = (x, y);
        }
        assert!(soft_limit(f32::INFINITY) < 0.9999 && soft_limit(f32::NAN).is_nan());
    }

    #[test]
    fn a_hiss_goes_and_every_sample_comes_back() {
        let mut d = Denoiser::new(2, 100).expect("the built-in model starts");
        let mut seed = 7;
        let (mut before, mut after) = (Vec::new(), Vec::new());
        // Two seconds in libobs's own odd-sized pieces (1024 frames), stereo.
        for _ in 0..94 {
            let mut l = hiss(1024, &mut seed);
            let mut r = l.clone();
            before.extend_from_slice(&l);
            d.run(&mut [&mut l, &mut r]);
            assert_eq!(l, r, "both channels alike");
            after.extend_from_slice(&l);
        }
        assert_eq!(before.len(), after.len(), "as many samples out as in");
        // The last second, once it has settled.
        let (b, a) = (rms(&before[48_000..]), rms(&after[48_000..]));
        assert!(a < b * 0.1, "the hiss is at least 20 dB quieter: {b} → {a}");
    }

    #[test]
    fn a_lighter_touch_leaves_some_of_it() {
        let mut d = Denoiser::new(1, 20).expect("the built-in model starts");
        let mut seed = 9;
        let (mut before, mut after) = (Vec::new(), Vec::new());
        for _ in 0..94 {
            let mut m = hiss(1024, &mut seed);
            before.extend_from_slice(&m);
            d.run(&mut [&mut m]);
            after.extend_from_slice(&m);
        }
        let (b, a) = (rms(&before[48_000..]), rms(&after[48_000..]));
        // 20% is at most 12 dB less: a quarter of the level or more.
        assert!(a > b * 0.2, "some of it stays: {b} → {a}");
    }

    #[test]
    fn strength_maps_to_a_limit() {
        // What every saved strength up to 50 has always meant (0.6 dB a step).
        assert_eq!(atten_db(50), 30.0);
        assert_eq!(atten_db(40), 24.0);
        assert_eq!(atten_db(33), 19.8);
        assert_eq!(atten_db(0), 0.6, "0 is read as 1");
        // The top is capped, never "unlimited" (100 dB), and has no cliff.
        assert_eq!(atten_db(100), 35.0);
        assert_eq!(atten_db(99), 34.9);
        assert_eq!(atten_db(65), 31.5);
        assert!((atten_db(100) - atten_db(99)).abs() < 0.2, "no 99 to 100 attenuation cliff");
        for s in 2..=100 {
            assert!(atten_db(s) > atten_db(s - 1), "strictly monotonic at {s}");
        }
    }

    use ferrocore::afx::{AudioEffect, AudioFilter};

    /// Hiss in `n`-frame blocks at the pace sound comes (the model starts on its own thread
    /// meanwhile): (energy in, energy out) over the last half, and each block's time in the
    /// mixer (ms).
    fn through(f: &mut AudioFilter, blocks: usize, n: usize) -> (f64, f64, Vec<f64>) {
        let mut seed = 1u32;
        let (mut before, mut after, mut took) = (0.0f64, 0.0f64, Vec::new());
        for block in 0..blocks {
            let mut l: Vec<f32> = (0..n)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 8) as f32 / (1 << 24) as f32 * 0.2 - 0.1
                })
                .collect();
            let mut r = l.clone();
            let energy: f64 = l.iter().map(|v| f64::from(*v).powi(2)).sum();
            let t = std::time::Instant::now();
            f.process([&mut l, &mut r]);
            took.push(t.elapsed().as_secs_f64() * 1000.0);
            if block >= blocks / 2 {
                before += energy;
                after += l.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>();
            }
            std::thread::sleep(std::time::Duration::from_secs_f64(n as f64 / 48_000.0));
        }
        (before, after, took)
    }

    #[test]
    fn our_engine_takes_the_noise_out() {
        super::install();
        let mut f = AudioFilter::new(AudioEffect::Denoise { strength: 100 }, 48_000);
        // Two seconds of steady hiss in 10 ms blocks; the second half measured.
        let (before, after, _) = through(&mut f, 200, 480);
        assert!(after < before * 0.1, "hiss mostly gone: {after:.3} of {before:.3}");
    }

    /// The mixer never waits for the model (it held the frame loop up to 100 ms a block when the
    /// computer was busy, 18 Nov): from the very first block, while it loads and once it runs,
    /// each block is back at once, and the delay the mixer files the sound by never changes.
    #[test]
    fn the_mixer_never_waits_for_it() {
        super::install();
        let mut f = AudioFilter::new(AudioEffect::Denoise { strength: 100 }, 48_000);
        let said = f.latency_frames();
        assert_eq!(said, MODEL_LATENCY + PIPE, "known before the model is made");
        let (before, after, took) = through(&mut f, 140, 1024);
        assert_eq!(f.latency_frames(), said, "the same delay once it runs");
        let worst = took.iter().copied().fold(0.0, f64::max);
        assert!(worst < 6.0, "no block waited: worst {worst:.2} ms");
        assert!(after < before * 0.1, "and it cleans: {after:.3} of {before:.3}");
    }

    /// Blocks come in twos, back to back, as the frame loop mixes them at 30 fps: the model's
    /// answers are still there in time (one block behind wasn't: a third went dry).
    #[test]
    fn blocks_in_pairs_stay_cleaned() {
        super::install();
        let mut f = AudioFilter::new(AudioEffect::Denoise { strength: 100 }, 48_000);
        // Ready first.
        let _ = through(&mut f, 30, 1024);
        let mut seed = 5u32;
        let (mut before, mut after) = (0.0f64, 0.0f64);
        for _ in 0..60 {
            for _ in 0..2 {
                let mut l: Vec<f32> = (0..1024)
                    .map(|_| {
                        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        (seed >> 8) as f32 / (1 << 24) as f32 * 0.2 - 0.1
                    })
                    .collect();
                let mut r = l.clone();
                before += l.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>();
                f.process([&mut l, &mut r]);
                after += l.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>();
            }
            std::thread::sleep(std::time::Duration::from_millis(42));
        }
        assert!(after < before * 0.1, "cleaned all along: {after:.3} of {before:.3}");
    }

    /// A strength change goes to the running model (no new model, no gap), and what comes out
    /// is still in step: a click goes in, and comes out exactly the delay said later.
    #[test]
    fn a_new_strength_keeps_it_running_and_in_step() {
        super::install();
        let mut f = AudioFilter::new(AudioEffect::Denoise { strength: 100 }, 48_000);
        let said = f.latency_frames();
        let _ = through(&mut f, 40, 1024);
        assert!(f.retune(AudioEffect::Denoise { strength: 60 }), "taken by the running one");
        // A buzz with a sharp start, after silence: where it comes out.
        let mut outl = Vec::new();
        let mut at = 0usize;
        for block in 0..60 {
            let mut l: Vec<f32> = (0..1024)
                .map(|i| {
                    let k = block * 1024 + i;
                    if k < 20_000 {
                        0.0
                    } else {
                        let t = k as f32 / 48_000.0;
                        (1..12).map(|h| (std::f32::consts::TAU * 140.0 * h as f32 * t).sin() / h as f32).sum::<f32>()
                            * 0.2
                    }
                })
                .collect();
            let mut r = l.clone();
            f.process([&mut l, &mut r]);
            outl.extend(l);
            at += 1024;
            std::thread::sleep(std::time::Duration::from_millis(21));
        }
        assert_eq!(at, outl.len());
        // Skip the pipeline tail of the preceding hiss before locating the buzz onset at k = 20_000.
        let first =
            outl.iter().enumerate().skip(said + 1024).find(|(_, v)| v.abs() > 0.02).map_or(usize::MAX, |(i, _)| i);
        assert!(first >= 20_000 + said - 64 && first <= 20_000 + said + 480, "came out at {first}, said {said}");
    }

    /// Deterministic fake denoiser with `MODEL_LATENCY` sample delay that halves amplitude
    /// (distinguishable from untouched dry audio) and can simulate a one-time mid-stream stall.
    struct MockEngine {
        delay: [VecDeque<f32>; 2],
        gain: f32,
        stall_on: Option<(usize, std::time::Duration)>,
        calls: usize,
    }

    impl MockEngine {
        fn new(gain: f32, stall_on: Option<(usize, std::time::Duration)>) -> Self {
            let z = || std::iter::repeat_n(0.0f32, MODEL_LATENCY).collect::<VecDeque<f32>>();
            Self { delay: [z(), z()], gain, stall_on, calls: 0 }
        }
    }

    impl WorkerEngine for MockEngine {
        fn latency(&self) -> usize {
            MODEL_LATENCY
        }
        fn set_strength(&mut self, _: u32) {}
        fn run(&mut self, planes: &mut [&mut [f32]]) {
            if let Some((at, dur)) = self.stall_on
                && self.calls == at
            {
                std::thread::sleep(dur);
            }
            self.calls += 1;
            for (c, p) in planes.iter_mut().take(2).enumerate() {
                for s in p.iter_mut() {
                    self.delay[c].push_back(*s * self.gain);
                    *s = self.delay[c].pop_front().unwrap_or(0.0);
                }
            }
        }
    }

    /// An 800 ms model load while real-time blocks arrive must not pile up an unbounded startup
    /// backlog or trip `gave_up`: wet output engages cleanly in exact phase once ready (M2-02).
    #[test]
    fn slow_startup_never_triggers_gave_up_and_engages_in_phase() {
        use ferrocore::afx::SoundPlugin as _;
        let Ok(mut p) = Plugin::start_with(65, READY_WITHIN, |_| {
            std::thread::sleep(std::time::Duration::from_millis(800));
            Ok(MockEngine::new(0.25, None))
        }) else {
            panic!("worker spawns");
        };
        let mut tail_rms = Vec::new();
        for b in 0..65 {
            let mut l = vec![0.4f32; 1024];
            let mut r = vec![0.4f32; 1024];
            assert!(p.process([&mut l, &mut r]), "block {b} must not give up");
            if b >= 55 {
                tail_rms.push(rms(&l));
            }
            std::thread::sleep(std::time::Duration::from_millis(21));
        }
        assert!(!p.gave_up, "slow startup must not permanently give up");
        assert!(p.ever_wet, "wet output engaged once model loaded");
        let avg = tail_rms.iter().sum::<f32>() / tail_rms.len() as f32;
        assert!((avg - 0.1).abs() < 0.01, "cleaned wet output (0.4 * 0.25 = 0.1), got {avg:.4}");
    }

    /// A model that fails to initialize (immediately or after a few startup blocks) must report
    /// zero latency and no handover buffer so `light_denoise` is not stuck behind an 82.7 ms delay.
    #[test]
    fn failed_init_leaves_zero_latency_and_no_handover() {
        use ferrocore::afx::SoundPlugin as _;
        let Ok(mut p) = Plugin::start_with::<MockEngine, _>(65, READY_WITHIN, |_| {
            std::thread::sleep(std::time::Duration::from_millis(30));
            Err("simulated model load failure".into())
        }) else {
            panic!("worker spawns");
        };
        let mut gave_up = false;
        for _ in 0..10 {
            let (mut l, mut r) = (vec![0.5f32; 1024], vec![0.5f32; 1024]);
            if !p.process([&mut l, &mut r]) {
                gave_up = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(gave_up, "failed init must hand over to light_denoise");
        assert_eq!(p.latency_frames(), 0, "failed init must report 0 latency, not 3968");
        assert!(p.hand_over().is_none(), "failed init must not hand over a dry delay line");
    }

    /// A transient CPU stall mid-stream drops oldest queued blocks instead of building a permanent
    /// backlog, then recovers cleanly back to wet output without giving up (M2-02).
    #[test]
    fn brief_cpu_stall_drops_oldest_and_recovers() {
        use ferrocore::afx::SoundPlugin as _;
        let Ok(mut p) = Plugin::start_with(65, READY_WITHIN, |_| {
            Ok(MockEngine::new(0.25, Some((10, std::time::Duration::from_millis(160)))))
        }) else {
            panic!("worker spawns");
        };
        let mut recovered = Vec::new();
        for b in 0..45 {
            let (mut l, mut r) = (vec![0.4f32; 1024], vec![0.4f32; 1024]);
            assert!(p.process([&mut l, &mut r]), "block {b} must not give up");
            if b >= 35 {
                recovered.push(rms(&l));
            }
            std::thread::sleep(std::time::Duration::from_millis(21));
        }
        assert!(!p.gave_up, "brief stall must not trigger permanent gave_up");
        let avg = recovered.iter().sum::<f32>() / recovered.len() as f32;
        assert!((avg - 0.1).abs() < 0.01, "recovered cleanly back to wet (0.1), got {avg:.4}");
    }

    /// A model that takes its sound in frames of `HOP` samples, like the real one: what's left over
    /// of a block waits for the next. Fed a ramp (every sample one step above the one before), a
    /// frame that is not a straight run of it was made of two stretches joined together.
    struct HopEngine {
        inp: Vec<f32>,
        out: VecDeque<f32>,
        joined: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        stall: Option<(usize, std::time::Duration)>,
        calls: usize,
    }

    const HOP: usize = 480;

    impl WorkerEngine for HopEngine {
        fn latency(&self) -> usize {
            MODEL_LATENCY
        }
        fn set_strength(&mut self, _: u32) {}
        fn gap(&mut self) {
            self.inp.clear();
        }
        fn run(&mut self, planes: &mut [&mut [f32]]) {
            if let Some((at, dur)) = self.stall
                && self.calls == at
            {
                std::thread::sleep(dur);
            }
            self.calls += 1;
            let n = planes[0].len();
            self.inp.extend_from_slice(planes[0]);
            while self.inp.len() >= HOP {
                let frame: Vec<f32> = self.inp.drain(..HOP).collect();
                if frame.windows(2).any(|w| (w[1] - w[0] - 1e-5).abs() > 1e-7) {
                    self.joined.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                self.out.extend(frame);
            }
            for s in planes[0].iter_mut() {
                *s = self.out.pop_front().unwrap_or(0.0);
            }
            for s in planes[1].iter_mut() {
                *s = 0.0;
            }
            let _ = n;
        }
    }

    /// A stall long enough to make the inbox drop its oldest blocks must not join the sound before
    /// the hole to the sound after it inside the model (a click through it, and its frames
    /// misaligned for a stretch): the worker tells it, and it forgets the half frame it held.
    #[test]
    fn a_dropped_block_is_not_spliced_into_the_one_after_it() {
        use ferrocore::afx::SoundPlugin as _;
        let joined = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let j = joined.clone();
        let Ok(mut p) = Plugin::start_with(65, READY_WITHIN, move |_| {
            Ok(HopEngine {
                inp: Vec::new(),
                out: std::iter::repeat_n(0.0, MODEL_LATENCY).collect(),
                joined: j,
                stall: Some((10, std::time::Duration::from_millis(200))),
                calls: 0,
            })
        }) else {
            panic!("worker spawns");
        };
        let mut k = 0usize;
        for _ in 0..40 {
            let mut l: Vec<f32> = (0..1024).map(|i| (k + i) as f32 * 1e-5).collect();
            let mut r = l.clone();
            k += 1024;
            assert!(p.process([&mut l, &mut r]));
            std::thread::sleep(std::time::Duration::from_millis(21));
        }
        // (The first frames after the model is ready may start mid-ramp: that is a start, not a join.)
        assert_eq!(joined.load(std::sync::atomic::Ordering::SeqCst), 0, "frames made of two stretches");
    }

    /// The output of a plugin whose answers are late now and then must not flip between the model's
    /// sound and the dry one every few blocks (a "tear": the noise floor pumping at block rate),
    /// nor jump between them in a millisecond: a wet to dry change takes at least 5 ms.
    #[test]
    fn late_answers_do_not_flip_flop_the_sound_or_jump_it() {
        use ferrocore::afx::SoundPlugin as _;
        // A model that is late (a stall) at several blocks close together.
        struct Flaky {
            inner: MockEngine,
            stalls: Vec<usize>,
        }
        impl WorkerEngine for Flaky {
            fn latency(&self) -> usize {
                MODEL_LATENCY
            }
            fn set_strength(&mut self, _: u32) {}
            fn run(&mut self, planes: &mut [&mut [f32]]) {
                if self.stalls.contains(&self.inner.calls) {
                    std::thread::sleep(std::time::Duration::from_millis(75));
                }
                self.inner.calls += 1;
                for (c, p) in planes.iter_mut().take(2).enumerate() {
                    for s in p.iter_mut() {
                        self.inner.delay[c].push_back(*s * self.inner.gain);
                        *s = self.inner.delay[c].pop_front().unwrap_or(0.0);
                    }
                }
            }
        }
        let Ok(mut p) = Plugin::start_with(65, READY_WITHIN, |_| {
            Ok(Flaky { inner: MockEngine::new(0.25, None), stalls: vec![14, 17, 20, 23, 26, 29, 32, 35, 38] })
        }) else {
            panic!("worker spawns");
        };
        let mut out = Vec::new();
        for _ in 0..70 {
            let (mut l, mut r) = (vec![0.4f32; 1024], vec![0.4f32; 1024]);
            assert!(p.process([&mut l, &mut r]));
            out.extend_from_slice(&l);
            std::thread::sleep(std::time::Duration::from_millis(21));
        }
        let from = 40 * 1024; // once the model is running
        let tail = &out[from..];
        // Dry is 0.4, the model's 0.1. No step between neighbours steeper than a 0.3 change in 5 ms.
        let steepest = tail.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(steepest <= 0.3 / 240.0, "a jump of {steepest:.4} in one sample: a change in under 5 ms");
        // And it doesn't cross back and forth: the times the level passes 0.25 (halfway).
        let crossings = tail.windows(2).filter(|w| (w[0] - 0.25) * (w[1] - 0.25) < 0.0).count();
        assert!(crossings <= 4, "the sound changed between dry and wet {crossings} times in {} ms", tail.len() / 48);
    }
}
