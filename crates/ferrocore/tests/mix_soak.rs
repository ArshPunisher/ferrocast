//! The mixer against devices that behave like real ones (the 19 Nov live failure, where tests
//! with perfectly even fakes passed and a cheap USB capture card broke the whole mix).
//!
//! Time is stepped, not waited for: a simulated two hours run in seconds. The engine's loop is
//! copied: every frame (60 per second) it hands the mixer what the devices delivered, then asks
//! for the blocks that are due (`Mixer::mix_until`).
//!
//! What's measured is the mixer's output only, through its public API: the mic is on track 1
//! and the card on track 2, so each can be judged alone.

use ferrocore::mix::{Block, Mixer, SourceSettings};

const RATE: u32 = 48_000;
/// 10 ms of sound, the piece a device hands over.
const PIECE: usize = 480;
const MIC: u64 = 1;
const CARD: u64 = 2;
/// The engine's frame loop at 60 fps.
const TICK_NS: u64 = 16_666_667;
/// Where simulated time starts (not zero: a real clock never starts there).
const T0: u64 = 10_000_000_000;
/// The engine's default wait for late sound (`EngineConfig::audio_latency_ms`).
const LATENCY_NS: u64 = 40_000_000;

fn secs(s: u64) -> u64 {
    s * 1_000_000_000
}

fn ms(m: u64) -> u64 {
    m * 1_000_000
}

/// A small seeded random source: the same run every time.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo + 1)
    }
}

/// A rising ramp, one second a round, so that any frame tells *when* it is from and a hole, a
/// repeat or a click shows as a step that isn't the ramp's own. Never 0 (0 means silence).
fn ramp(frame: u64, lo: f32, span: f32) -> f32 {
    lo + span * (frame % u64::from(RATE)) as f32 / RATE as f32
}

/// How the mixer meets a device's sound.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Stamp {
    /// The device's own times, true to the host clock (AVFoundation).
    Device,
    /// When the piece arrived: the newest ends now, the ones before it a piece earlier each
    /// (what `sound_in::keep` does on Windows and Linux).
    Arrival,
}

/// A steady microphone: a piece every 10 ms, handed over at the next frame tick.
struct Mic {
    delivered: u64,
    /// No sound from this time to this time (the owner pulled the cable).
    off: Option<(u64, u64)>,
}

impl Mic {
    fn poll(&mut self, now: u64, rate: u32, mut sound: impl FnMut(Vec<f32>, u64, u32)) {
        let piece_ns = PIECE as u64 * 1_000_000_000 / u64::from(rate);
        while T0 + (self.delivered + 1) * piece_ns <= now {
            let begin = T0 + self.delivered * piece_ns;
            self.delivered += 1;
            if self.off.is_some_and(|(a, b)| begin >= a && begin < b) {
                continue;
            }
            let first = self.delivered * PIECE as u64 - PIECE as u64;
            let pcm = (0..PIECE as u64).map(|i| ramp(first + i, 0.1, 0.8)).collect();
            sound(pcm, begin, rate);
        }
    }
}

/// A cheap capture card: sound in bursts, a clock off by `drift_ppm`, now and then a stall in
/// which nothing comes and then everything at once.
struct Card {
    rng: Rng,
    drift: f64,
    stamp: Stamp,
    delivered: u64,
    next_burst: u64,
    stalls: bool,
    /// Each time a delivery happened: (previous delivery, this one, whether it was a stall).
    deliveries: Vec<(u64, u64, bool)>,
    last: u64,
}

impl Card {
    fn new(seed: u64, drift_ppm: f64, stamp: Stamp, stalls: bool) -> Self {
        Self {
            rng: Rng(seed),
            drift: drift_ppm / 1e6,
            stamp,
            delivered: 0,
            next_burst: T0,
            stalls,
            deliveries: Vec::new(),
            last: T0,
        }
    }

    /// What the card hands over at `now` (pieces and their times), if it's time.
    fn poll(&mut self, now: u64) -> Vec<(Vec<f32>, u64)> {
        if now < self.next_burst {
            return Vec::new();
        }
        let per_sec = f64::from(RATE) * (1.0 + self.drift);
        let total = ((now - T0) as f64 * per_sec / 1e9) as u64 / PIECE as u64;
        let n = total - self.delivered;
        let mut out = Vec::new();
        for k in 0..n {
            let p = self.delivered + k;
            let pcm = (0..PIECE as u64).map(|i| ramp(p * PIECE as u64 + i, 0.1, 0.8)).collect();
            let ts = match self.stamp {
                Stamp::Device => T0 + (p as f64 * PIECE as f64 * 1e9 / per_sec) as u64,
                Stamp::Arrival => now.saturating_sub((n - k) * ms(10)),
            };
            out.push((pcm, ts));
        }
        self.delivered = total;
        let stall = self.stalls && self.rng.range(0, 899) == 0;
        self.deliveries.push((self.last, now, stall));
        self.last = now;
        // The next burst is 40–120 ms on, or after a stall 400 ms on.
        self.next_burst = now + if stall { ms(400) } else { ms(self.rng.range(40, 120)) };
        out
    }
}

fn mixer() -> Mixer {
    let mut m = Mixer::new(RATE, LATENCY_NS);
    // The mic only on track 1, the card only on track 2: each is heard alone in its track.
    m.set(MIC, SourceSettings { tracks: 0b01, ..SourceSettings::default() });
    m.set(CARD, SourceSettings { tracks: 0b10, ..SourceSettings::default() });
    m
}

/// How many frames of source `id` wait in the mixer (read from its `Debug`: the queue itself is
/// private, and this test is on the public API only).
fn queued_frames(m: &Mixer, id: u64) -> u64 {
    let text = format!("{m:?}");
    let queued = text.find(&format!("{id}: Source")).and_then(|at| {
        let rest = &text[at..];
        let q = rest.find("queued: ")? + "queued: ".len();
        rest[q..].chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
    });
    let Some(queued) = queued else { panic!("the mixer's Debug doesn't show source {id}'s queue: {text}") };
    queued
}

/// Judges the mic track: every frame is where its time says (within a millisecond), no frame is
/// missing, repeated or changed (the ramp rises by one step a frame), blocks are never silent.
/// Stretches of sound (`runs`, begin and end in ns) are kept for tests where the mic stops.
#[derive(Default)]
struct MicJudge {
    last: Option<f32>,
    frames: u64,
    glitches: Vec<String>,
    worst_shift_frames: i64,
    silent_blocks: u64,
    runs: Vec<(u64, u64)>,
}

impl MicJudge {
    fn block(&mut self, b: &Block, rate: u32) {
        let track = &b.tracks[0];
        let step = 0.8 / rate as f32;
        if track.as_chunks::<2>().0.iter().all(|f| f[0] == 0.0) {
            self.silent_blocks += 1;
        }
        for (i, f) in track.as_chunks::<2>().0.iter().enumerate() {
            let v = f[0];
            let t = b.ts_ns + i as u64 * 1_000_000_000 / u64::from(rate);
            if v == 0.0 {
                self.last = None;
                continue;
            }
            match self.runs.last_mut() {
                Some(r) if self.last.is_some() => r.1 = t,
                _ => self.runs.push((t, t)),
            }
            // Where the frame should be, from its own time: the ramp's value at that time.
            if i % 128 == 0 {
                let want = ((t - T0) as f64 * f64::from(rate) / 1e9).round() as i64;
                let got = (f64::from(v - 0.1) / 0.8 * f64::from(rate)).round() as i64;
                let mut shift = (got - want).rem_euclid(i64::from(rate));
                if shift > i64::from(rate) / 2 {
                    shift -= i64::from(rate);
                }
                if shift.abs() > self.worst_shift_frames.abs() {
                    self.worst_shift_frames = shift;
                }
            }
            if let Some(p) = self.last {
                let d = v - p;
                // The ramp's own wrap (once a second) isn't a fault.
                let ok = (d > step * 0.5 && d < step * 1.5) || d < -0.5;
                if !ok && self.glitches.len() < 20 {
                    self.glitches
                        .push(format!("at {:.3}s: step {d:.6} (the ramp's is {step:.6})", (t - T0) as f64 / 1e9));
                }
            }
            self.last = Some(v);
            self.frames += 1;
        }
    }
}

/// One stretch of silence in the card's track: where it begins and how long (ns).
#[derive(Debug, Clone, Copy)]
struct Hole {
    at: u64,
    len: u64,
}

#[derive(Default)]
struct CardJudge {
    run_from: Option<u64>,
    holes: Vec<Hole>,
    frames: u64,
    last: Option<f32>,
    /// Where the card's sound jumped (a piece dropped or repeated: a click), and by how many ms
    /// of its ramp.
    tears: Vec<(u64, f64)>,
}

impl CardJudge {
    fn block(&mut self, b: &Block, rate: u32) {
        for (i, f) in b.tracks[1].as_chunks::<2>().0.iter().enumerate() {
            let t = b.ts_ns + i as u64 * 1_000_000_000 / u64::from(rate);
            if f[0].abs() < 0.05 {
                self.run_from.get_or_insert(t);
                self.last = None;
                continue;
            }
            if let Some(p) = self.last {
                let d = f[0] - p;
                let step = 0.8 / rate as f32;
                if !((d > step * 0.5 && d < step * 1.5) || d < -0.5) {
                    self.tears.push((t, f64::from(d / step) * 1000.0 / f64::from(rate)));
                }
            }
            self.last = Some(f[0]);
            if let Some(from) = self.run_from.take()
                && t - from >= ms(5)
            {
                self.holes.push(Hole { at: from, len: t - from });
            }
            self.frames += 1;
        }
    }
}

struct Soak {
    stamp: Stamp,
    drift_ppm: f64,
    stalls: bool,
    /// How long the run is, in simulated seconds.
    secs: u64,
    /// The mic is unplugged from this second to that one.
    mic_off: Option<(u64, u64)>,
}

const TWO_HOURS: u64 = 2 * 3600;

struct SoakResult {
    mic: MicJudge,
    card: CardJudge,
    deliveries: Vec<(u64, u64, bool)>,
    max_latency_ns: u64,
    /// The highest latency seen after the first minute, per hour of the run (when it shrank).
    latency_at_end_ns: u64,
    ticks_at_cap: u64,
    max_queue: [u64; 2],
}

fn soak(cfg: &Soak) -> SoakResult {
    let mut m = mixer();
    let mut mic = Mic { delivered: 0, off: cfg.mic_off.map(|(a, b)| (T0 + secs(a), T0 + secs(b))) };
    let mut card = Card::new(0x9E37_79B9_7F4A_7C15, cfg.drift_ppm, cfg.stamp, cfg.stalls);
    let (mut mj, mut cj) = (MicJudge::default(), CardJudge::default());
    let end = T0 + secs(cfg.secs);
    let (mut now, mut tick) = (T0, 0u64);
    let (mut max_latency, mut at_cap, mut max_queue) = (0u64, 0u64, [0u64; 2]);
    while now < end {
        now += TICK_NS;
        tick += 1;
        mic.poll(now, RATE, |pcm, ts, rate| m.push(MIC, &pcm, 1, rate, ts));
        for (pcm, ts) in card.poll(now) {
            m.push(CARD, &pcm, 1, RATE, ts);
        }
        for b in m.mix_until(now) {
            // The first seconds are the mixer finding its feet.
            if b.ts_ns > T0 + secs(3) {
                mj.block(&b, RATE);
                cj.block(&b, RATE);
            }
        }
        max_latency = max_latency.max(m.latency_ns());
        if m.latency_ns() >= secs(1) {
            at_cap += 1;
        }
        if tick % 60 == 0 {
            max_queue[0] = max_queue[0].max(queued_frames(&m, MIC));
            max_queue[1] = max_queue[1].max(queued_frames(&m, CARD));
        }
    }
    SoakResult {
        mic: mj,
        card: cj,
        deliveries: card.deliveries,
        max_latency_ns: max_latency,
        latency_at_end_ns: m.latency_ns(),
        ticks_at_cap: at_cap,
        max_queue,
    }
}

/// The stretches the card was silent for no reason: a hole is excused only inside a stall (the
/// time between two deliveries that was a stall, and a short while after it).
fn unexcused_holes(r: &SoakResult) -> Vec<Hole> {
    let stalls: Vec<(u64, u64)> = r.deliveries.iter().filter(|d| d.2).map(|d| (d.0, d.1)).collect();
    r.card
        .holes
        .iter()
        .copied()
        .filter(|h| !stalls.iter().any(|(a, b)| h.at + h.len + ms(100) >= *a && h.at <= *b + ms(700)))
        .collect()
}

fn judge(name: &str, cfg: &Soak) {
    let r = soak(cfg);
    let stalls = r.deliveries.iter().filter(|d| d.2).count();
    eprintln!(
        "{name}: {} stalls, mic worst shift {} frames, {} holes ({} unexcused), latency max {} ms end {} ms, queues {:?}",
        stalls,
        r.mic.worst_shift_frames,
        r.card.holes.len(),
        unexcused_holes(&r).len(),
        r.max_latency_ns / 1_000_000,
        r.latency_at_end_ns / 1_000_000,
        r.max_queue
    );
    assert!(r.mic.frames > cfg.secs * u64::from(RATE) / 2, "{name}: the mic was heard");
    assert_eq!(r.mic.silent_blocks, 0, "{name}: the mic was never silent for a block");
    assert!(r.mic.glitches.is_empty(), "{name}: the mic was cut or torn: {:?}", r.mic.glitches);
    assert!(
        r.mic.worst_shift_frames.abs() <= 48,
        "{name}: the mic was {} frames off its own time (more than 1 ms)",
        r.mic.worst_shift_frames
    );
    assert_eq!(r.ticks_at_cap, 0, "{name}: the mixer's wait sat at its 1 s cap");
    // The wait may rise for late sound (a burst is up to 120 ms late) but never past the point
    // where a source is timed by itself (300 ms) and a little room.
    assert!(r.max_latency_ns <= ms(340), "{name}: the wait rose to {} ms", r.max_latency_ns / 1_000_000);
    let holes = unexcused_holes(&r);
    assert!(holes.is_empty(), "{name}: the card has holes outside its stalls: {holes:?}");
    // Memory: a second of sound waiting is already far more than the wait needs.
    assert!(r.max_queue.iter().all(|q| *q <= u64::from(RATE)), "{name}: queues grew: {:?} frames", r.max_queue);
}

#[test]
fn two_hours_with_a_bursty_card_stamped_on_arrival() {
    let cfg = Soak { stamp: Stamp::Arrival, drift_ppm: 300.0, stalls: true, secs: TWO_HOURS, mic_off: None };
    judge("arrival +300ppm", &cfg);
}

#[test]
fn two_hours_with_a_bursty_card_stamped_by_the_device() {
    let cfg = Soak { stamp: Stamp::Device, drift_ppm: 300.0, stalls: true, secs: TWO_HOURS, mic_off: None };
    judge("device +300ppm", &cfg);
}

/// The mic never notices the card's drift or stalls; and the card's own sound: a clock that runs
/// fast makes the mixer drop 70 ms of it each time the stamps fall that far behind the sound
/// (a jump in what's heard, a click), one that runs slow leaves a gap of what it fell short by.
/// Both come from a device clock that isn't followed (the mixer places sound by stamps only).
#[test]
#[ignore = "finding S1-F1: a card's clock drift isn't followed: a click every ~4 min fast, a dropout every ~70 s slow"]
fn a_drifting_card_gives_no_clicks_and_no_dropouts() {
    let mut bad = Vec::new();
    for (name, stamp, ppm) in [
        ("arrival +300", Stamp::Arrival, 300.0),
        ("device +300", Stamp::Device, 300.0),
        ("device -300", Stamp::Device, -300.0),
    ] {
        let r = soak(&Soak { stamp, drift_ppm: ppm, stalls: false, secs: 600, mic_off: None });
        if !r.card.tears.is_empty() {
            bad.push(format!(
                "{name}: the card's sound jumped {} times: {:?}",
                r.card.tears.len(),
                &r.card.tears[..r.card.tears.len().min(3)]
            ));
        }
        if !r.card.holes.is_empty() {
            bad.push(format!(
                "{name}: the card dropped out {} times: {:?}",
                r.card.holes.len(),
                &r.card.holes[..r.card.holes.len().min(3)]
            ));
        }
    }
    assert!(bad.is_empty(), "{bad:#?}");
}

#[test]
fn a_mic_that_stops_for_3_s_and_comes_back_is_heard_again_at_once() {
    // The same card throughout, so the mic's return is seen against a mix that's busy.
    let with = |mic_off| Soak { stamp: Stamp::Arrival, drift_ppm: 0.0, stalls: false, secs: 60, mic_off };
    let base = soak(&with(None));
    let r = soak(&with(Some((20, 23))));
    // The card heard throughout, as the baseline: nothing of it was lost by the mic's trouble.
    assert_eq!(r.card.holes.len(), base.card.holes.len(), "the card's holes changed: {:?}", r.card.holes);
    assert_eq!(r.card.tears.len(), base.card.tears.len(), "the card's sound jumped: {:?}", r.card.tears);
    // Two stretches of mic sound, and in between nothing (no stale sound repeated).
    assert_eq!(r.mic.runs.len(), 2, "the mic's stretches of sound: {:?}", r.mic.runs);
    let (a, b) = (r.mic.runs[0], r.mic.runs[1]);
    assert!(
        a.1.abs_diff(T0 + secs(20)) <= ms(2),
        "the mic's sound stopped when it did: {} ms off",
        a.1.abs_diff(T0 + secs(20)) / 1_000_000
    );
    assert!(
        b.0.abs_diff(T0 + secs(23)) <= ms(2),
        "the mic came back when it did: {} ms off",
        b.0.abs_diff(T0 + secs(23)) / 1_000_000
    );
    assert!(r.mic.glitches.is_empty(), "the mic's sound was torn: {:?}", r.mic.glitches);
    assert!(r.mic.worst_shift_frames.abs() <= 48, "the mic was {} frames off its time", r.mic.worst_shift_frames);
    // Its return asks for no longer wait: nothing came late.
    assert_eq!(r.max_latency_ns, base.max_latency_ns, "the mic's return changed the wait");
}

/// A sine: any step bigger than its own is a click, a gap is a stretch of nothing.
#[derive(Default)]
struct SineJudge {
    last: Option<f32>,
    worst_step: f32,
    worst_at: u64,
    quiet_run: u32,
    longest_quiet: u32,
}

impl SineJudge {
    fn block(&mut self, b: &Block, rate: u32) {
        for (i, f) in b.tracks[0].as_chunks::<2>().0.iter().enumerate() {
            if let Some(p) = self.last {
                let d = (f[0] - p).abs();
                if d > self.worst_step {
                    self.worst_step = d;
                    self.worst_at = b.ts_ns + i as u64 * 1_000_000_000 / u64::from(rate);
                }
            }
            self.last = Some(f[0]);
            self.quiet_run = if f[0].abs() < 0.01 { self.quiet_run + 1 } else { 0 };
            self.longest_quiet = self.longest_quiet.max(self.quiet_run);
        }
    }
}

#[test]
fn a_mic_that_changes_rate_mid_stream_has_no_click_and_no_gap() {
    const HZ: f64 = 440.0;
    const AMP: f32 = 0.5;
    let mut m = Mixer::new(RATE, LATENCY_NS);
    m.set(MIC, SourceSettings::default());
    let switch = T0 + secs(20);
    let (mut now, mut next_piece) = (T0, T0);
    let mut j = SineJudge::default();
    while now < T0 + secs(40) {
        now += TICK_NS;
        // 10 ms pieces: 441 frames at 44.1 kHz, then 480 at 48 kHz.
        loop {
            let rate: u32 = if next_piece < switch { 44_100 } else { 48_000 };
            let frames = (rate / 100) as u64;
            let end = next_piece + 10_000_000;
            if end > now {
                break;
            }
            let pcm: Vec<f32> = (0..frames)
                .map(|i| {
                    let t = (next_piece - T0) as f64 / 1e9 + i as f64 / f64::from(rate);
                    AMP * (2.0 * std::f64::consts::PI * HZ * t).sin() as f32
                })
                .collect();
            m.push(MIC, &pcm, 1, rate, next_piece);
            next_piece = end;
        }
        for b in m.mix_until(now) {
            if b.ts_ns > T0 + secs(3) {
                j.block(&b, RATE);
            }
        }
    }
    // The steepest a clean 440 Hz sine rises per frame, and a little for the resampler.
    let steepest = AMP * (2.0 * std::f64::consts::PI * HZ / f64::from(RATE)) as f32;
    eprintln!(
        "rate change: worst step {:.4} (a sine's {:.4}) at {:.3}s, longest quiet {} frames",
        j.worst_step,
        steepest,
        (j.worst_at - T0) as f64 / 1e9,
        j.longest_quiet
    );
    assert!(
        j.worst_step <= steepest * 1.25,
        "a click at {:.3}s (the switch is at 20 s): a step of {:.4}, a sine's is {:.4}",
        (j.worst_at - T0) as f64 / 1e9,
        j.worst_step,
        steepest
    );
    // 120 frames (2.5 ms) of near zero is more than a sine's own crossing.
    assert!(j.longest_quiet < 120, "a gap of {} frames", j.longest_quiet);
    assert_eq!(m.latency_ns(), LATENCY_NS, "changing rate changed the wait");
}
