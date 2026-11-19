//! Sound in through cpal (where there's no capture API of ours: Windows): a microphone, or
//! what the computer plays (WASAPI's loopback: an output device opened as an input). Whatever
//! the device's format, the engine gets 48 kHz stereo floats, stamped with its clock.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::AvError;
use crate::engine::now_ns;

/// Sound kept for the engine: 48 kHz stereo pieces with when they came, how many came in all
/// (the watchdog's sign of life), and why the device stopped if it did.
#[derive(Debug, Default)]
struct Kept {
    pieces: Mutex<VecDeque<(Vec<f32>, u64)>>,
    count: AtomicU64,
    fault: Mutex<Option<String>>,
}

type Pieces = Arc<Kept>;

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
    /// One app's sound, by its process id (its child processes too).
    #[cfg(target_os = "windows")]
    Process(u32),
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
        #[cfg(target_os = "windows")]
        if let Which::Process(pid) = which {
            let thread = std::thread::Builder::new()
                .name("ferrocast-app-sound".into())
                .spawn(move || {
                    let mut conv = Convert::new(2, RATE);
                    let s2 = s.clone();
                    crate::win::app_sound::run(pid, st.clone(), ready_tx, |data| keep(&s2, conv.push(data)));
                    // Back while not asked to stop: the app's sound ended (it quit, or Windows
                    // let go of it). Said, so the watchdog starts it again.
                    if !st.load(Ordering::Acquire) {
                        fault(&s, "the app's sound stopped".into());
                    }
                })
                .map_err(|e| AvError::Source(e.to_string()))?;
            return Self::started(sound, stop, thread, ready_rx);
        }
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
        Self::started(sound, stop, thread, ready_rx)
    }

    /// Waits for the thread to say whether its sound opened.
    fn started(
        sound: Pieces,
        stop: Arc<AtomicBool>,
        thread: JoinHandle<()>,
        ready_rx: std::sync::mpsc::Receiver<Result<(), String>>,
    ) -> Result<Self, AvError> {
        match ready_rx.recv_timeout(Duration::from_secs(8)) {
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
        self.sound.pieces.lock().map(|mut q| q.drain(..).collect()).unwrap_or_default()
    }

    /// Pieces received since the start: a microphone sends all the time, so a count that stops
    /// growing means it stopped.
    pub fn received(&self) -> u64 {
        self.sound.count.load(Ordering::Relaxed)
    }

    /// Why the device stopped, if it did (unplugged, its format changed, the app quit). Before
    /// 20 Nov this was only logged and the watchdog never saw it: a Windows mic unplugged and
    /// plugged back stayed silent for the rest of the stream (OBS's WASAPI source reconnects).
    pub fn ended(&self) -> Option<String> {
        self.sound.fault.lock().ok().and_then(|f| f.clone())
    }
}

/// Whether an error from cpal means the stream stopped (it must be opened again). An overrun
/// (WASAPI's "data discontinuity" after a stall), a reroute the stream followed, or real-time
/// priority refused are reported too, but the stream goes on: restarting a working mic for those
/// would cut it (cpal 0.18 `wasapi/stream.rs` `process_input`).
fn stops(kind: cpal::ErrorKind) -> bool {
    !matches!(kind, cpal::ErrorKind::Xrun | cpal::ErrorKind::DeviceChanged | cpal::ErrorKind::RealtimeDenied)
}

/// Sound that starts itself again when its device stops (speakers unplugged, the default
/// changed), in the background, beside a picture that goes on: restarting the whole capture for
/// its sound blinked the video (agy's review, 20 Nov). Without a picture (`heals` false) it
/// doesn't: its stop is said by [`Heal::ended`] and the watchdog starts the source again.
#[derive(Debug)]
pub struct Heal {
    which: Which,
    heals: bool,
    slot: Mutex<Slot>,
}

#[derive(Debug, Default)]
struct Slot {
    now: Option<SoundIn>,
    /// A start on its own thread (it can wait seconds for the device); the old one is let go
    /// there too, so the frame loop never waits for it.
    starting: Option<JoinHandle<Result<SoundIn, AvError>>>,
    next_try: u64,
    tries: u32,
    /// Pieces counted by sound that was replaced, so the count keeps growing.
    before: u64,
}

impl Heal {
    pub fn new(sound: SoundIn, which: Which, heals: bool) -> Self {
        Heal { which, heals, slot: Mutex::new(Slot { now: Some(sound), ..Slot::default() }) }
    }

    /// Sound since the last call; first a look at whether it must start again.
    pub fn take_sound(&self) -> Vec<(Vec<f32>, u64)> {
        let Ok(mut slot) = self.slot.lock() else { return Vec::new() };
        if self.heals {
            self.tend(&mut slot, now_ns());
        }
        slot.now.as_ref().map(SoundIn::take_sound).unwrap_or_default()
    }

    fn tend(&self, slot: &mut Slot, now: u64) {
        if slot.starting.as_ref().is_some_and(JoinHandle::is_finished) {
            match slot.starting.take().map(JoinHandle::join) {
                Some(Ok(Ok(s))) => {
                    tracing::info!("a sound started again");
                    slot.now = Some(s);
                    slot.tries = 0;
                }
                other => {
                    let e = match other {
                        Some(Ok(Err(e))) => e.to_string(),
                        _ => "its start failed".into(),
                    };
                    tracing::warn!(error = %e, "a sound didn't start again; it's tried later");
                    slot.next_try = now + retry_ns(slot.tries);
                    slot.tries += 1;
                }
            }
        }
        let stopped = slot.now.as_ref().is_none_or(|s| s.ended().is_some());
        if stopped && slot.starting.is_none() && now >= slot.next_try {
            let old = slot.now.take();
            if let Some(o) = &old {
                slot.before += o.received();
                tracing::warn!(why = ?o.ended(), "a sound stopped: it's started again");
            }
            let which = self.which.clone();
            slot.starting = std::thread::Builder::new()
                .name("ferrocast-sound-heal".into())
                .spawn(move || {
                    drop(old);
                    SoundIn::start(which)
                })
                .ok();
        }
    }

    /// Pieces received since the start, across restarts.
    pub fn received(&self) -> u64 {
        self.slot.lock().map(|s| s.before + s.now.as_ref().map_or(0, SoundIn::received)).unwrap_or(0)
    }

    /// Why it stopped, when it doesn't start itself again.
    pub fn ended(&self) -> Option<String> {
        if self.heals {
            return None;
        }
        self.slot.lock().ok()?.now.as_ref()?.ended()
    }
}

/// The wait before try `n` at starting sound again: 2 s, doubling, 30 s at most.
fn retry_ns(n: u32) -> u64 {
    (2_000_000_000u64 << n.min(4)).min(30_000_000_000)
}

/// Notes why the sound stopped (the first reason is kept).
fn fault(sound: &Kept, why: String) {
    if let Ok(mut f) = sound.fault.lock()
        && f.is_none()
    {
        *f = Some(why);
    }
}

/// Finished pieces into `sound`, each stamped with when it began: the newest ends now, the
/// ones before it a piece earlier each (the oldest let go past about 5 s). One stamp for all
/// (a device handing over several at once) made them look like a jump back in time, and the
/// mixer threw away what it had queued: a crackle at every burst (19 Nov).
fn keep(sound: &Pieces, pieces: Vec<Vec<f32>>) {
    if pieces.is_empty() {
        return;
    }
    let now = now_ns();
    let n = pieces.len() as u64;
    sound.count.fetch_add(n, Ordering::Relaxed);
    if let Ok(mut q) = sound.pieces.lock() {
        for (k, p) in pieces.into_iter().enumerate() {
            let len = (p.len() / 2) as u64 * 1_000_000_000 / u64::from(RATE);
            q.push_back((p, now.saturating_sub((n - k as u64) * len)));
        }
        while q.len() > MAX_PIECES {
            q.pop_front();
        }
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
        #[cfg(target_os = "windows")]
        Which::Process(_) => return Err("one app's sound doesn't come through cpal".into()),
    };
    let cfg = match which {
        Which::Loopback => dev.default_output_config(),
        _ => dev.default_input_config(),
    }
    .map_err(|e| format!("its format couldn't be read ({e})"))?;
    let mut conv = Convert::new(usize::from(cfg.channels()), cfg.sample_rate());
    let errs = sound.clone();
    let mut keep = move |data: &[f32]| keep(&sound, conv.push(data));
    let err = move |e: cpal::Error| {
        if !stops(e.kind()) {
            // A glitch (a buffer overrun) or a reroute the stream followed: it goes on.
            tracing::debug!(%e, "a sound input glitched");
            return;
        }
        tracing::warn!(%e, "a sound input stopped");
        fault(&errs, format!("it stopped ({e})"));
    };
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

    /// Pieces handed over together are stamped one after another, the newest ending now.
    #[test]
    fn a_burst_of_pieces_gets_times_one_after_another() {
        let sound: Pieces = Arc::default();
        keep(&sound, vec![vec![0.0; 2048], vec![0.0; 2048], vec![0.0; 2048]]);
        let q: Vec<u64> = sound.pieces.lock().map(|q| q.iter().map(|p| p.1).collect()).unwrap_or_default();
        let piece = 1024 * 1_000_000_000 / u64::from(RATE);
        assert_eq!(q.len(), 3);
        assert_eq!(q[1] - q[0], piece);
        assert_eq!(q[2] - q[1], piece);
        assert!(now_ns() - q[2] >= piece, "the newest began a piece ago");
    }

    /// The watchdog's signs: pieces are counted, and a device that stopped says why (the first
    /// reason is kept). Without these a Windows mic that was unplugged stayed silent for good.
    #[test]
    fn a_device_that_stops_says_so_and_pieces_are_counted() {
        let sound: Pieces = Arc::default();
        let s = SoundIn { sound: sound.clone(), stop: Arc::default(), thread: None };
        assert_eq!((s.received(), s.ended()), (0, None));
        keep(&sound, vec![vec![0.0; 2048], vec![0.0; 2048]]);
        assert_eq!(s.received(), 2);
        let _ = s.take_sound();
        assert_eq!(s.received(), 2, "taking the sound doesn't reset the count");
        fault(&sound, "it stopped (device not available)".into());
        fault(&sound, "a later reason".into());
        assert_eq!(s.ended().as_deref(), Some("it stopped (device not available)"));
    }

    /// A glitch isn't a stop; a lost device is.
    #[test]
    fn only_errors_that_end_the_stream_count_as_stopped() {
        use cpal::ErrorKind as K;
        for k in [K::Xrun, K::DeviceChanged, K::RealtimeDenied] {
            assert!(!stops(k), "{k:?} goes on");
        }
        for k in [K::DeviceNotAvailable, K::StreamInvalidated, K::BackendError, K::DeviceBusy] {
            assert!(stops(k), "{k:?} stops");
        }
    }

    fn faulted() -> SoundIn {
        let sound: Pieces = Arc::default();
        fault(&sound, "it stopped (device not available)".into());
        SoundIn { sound, stop: Arc::default(), thread: None }
    }

    /// Beside a picture, sound that stopped starts again on its own (here: a device that isn't
    /// there, so the start fails and waits longer); alone, it says it stopped instead. No real
    /// device is opened: the name matches none.
    #[test]
    fn sound_beside_a_picture_starts_itself_again() {
        let gone = Which::Input(Some("ferrocast-test-no-such-device".into()));
        let alone = Heal::new(faulted(), gone.clone(), false);
        assert!(alone.ended().is_some(), "sound alone: the watchdog's job");
        let _ = alone.take_sound();
        assert!(alone.slot.lock().unwrap().starting.is_none(), "and it doesn't start itself");

        let heal = Heal::new(faulted(), gone, true);
        assert_eq!(heal.ended(), None, "beside a picture: never ends the capture");
        let _ = heal.take_sound();
        let handle = heal.slot.lock().unwrap().starting.take();
        assert!(handle.as_ref().is_some(), "a start is under way");
        heal.slot.lock().unwrap().starting = handle;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while heal.slot.lock().unwrap().starting.as_ref().is_some_and(|h| !h.is_finished()) {
            assert!(std::time::Instant::now() < deadline, "the start never finished");
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = heal.take_sound();
        let slot = heal.slot.lock().unwrap();
        assert_eq!(slot.tries, 1, "the failed start counts");
        assert!(slot.next_try > now_ns(), "and the next one waits");
        assert_eq!((retry_ns(0), retry_ns(9)), (2_000_000_000, 30_000_000_000));
    }

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
