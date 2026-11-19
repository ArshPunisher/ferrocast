//! NDI in the Studio: an NDI Source (OBS's DistroAV `ndi_source`) read into a fed source with
//! its sound, reconnecting when it drops; and what's on air sent out as an NDI source.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ferrocore::engine::{Feed, now_ns};

use crate::ffi::ndi::{Got, Receiver, Sender};
use crate::own::Shared;

/// A thread that stops when dropped.
#[derive(Debug)]
struct Worker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn spawn(name: &str, f: impl FnOnce(&AtomicBool) + Send + 'static) -> Result<Worker, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let stop = stop.clone();
        std::thread::Builder::new().name(name.into()).spawn(move || f(&stop)).map_err(|e| e.to_string())?
    };
    Ok(Worker { stop, thread: Some(thread) })
}

/// One NDI source read into a fed source. Dropping it lets go.
#[derive(Debug)]
pub(crate) struct NdiIn {
    feed: Arc<Mutex<Feed>>,
    _worker: Worker,
}

impl NdiIn {
    /// Reads the NDI source called `name` into `feed`, connecting again (every 2 s) when it
    /// isn't there or drops.
    pub(crate) fn start(name: &str, feed: Feed) -> Result<NdiIn, String> {
        crate::ffi::ndi::available()?;
        let feed = Arc::new(Mutex::new(feed));
        let worker = {
            let (name, feed) = (name.to_string(), feed.clone());
            spawn("ferrocast-ndi-in", move |stop| {
                while !stop.load(Ordering::Acquire) {
                    match Receiver::connect(&name) {
                        Ok(mut rx) => {
                            let mut heard = Instant::now();
                            while !stop.load(Ordering::Acquire) {
                                match rx.next(100) {
                                    Got::Picture(p) => {
                                        heard = Instant::now();
                                        feed.lock().unwrap_or_else(|e| e.into_inner()).picture(p);
                                    }
                                    Got::Sound(pcm, rate) => {
                                        heard = Instant::now();
                                        let span = (pcm.len() / 2) as u64 * 1_000_000_000 / u64::from(rate.max(1));
                                        let f = feed.lock().unwrap_or_else(|e| e.into_inner()).clone();
                                        f.sound(pcm, rate, now_ns().saturating_sub(span));
                                    }
                                    Got::Nothing if heard.elapsed() < Duration::from_secs(10) => {}
                                    // Nothing for 10 s, or an error: connect again.
                                    Got::Nothing | Got::Lost => break,
                                }
                            }
                        }
                        Err(e) => tracing::warn!(source = %name, "NDI: {e}"),
                    }
                    let until = Instant::now() + Duration::from_secs(2);
                    while !stop.load(Ordering::Acquire) && Instant::now() < until {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            })?
        };
        Ok(NdiIn { feed, _worker: worker })
    }

    /// Our engine's source changed (the collection was opened again): its new feed.
    pub(crate) fn set_feed(&self, feed: Feed) {
        *self.feed.lock().unwrap_or_else(|e| e.into_inner()) = feed;
    }
}

/// What's on air sent out as an NDI source. Dropping it stops.
#[derive(Debug)]
pub struct NdiOut {
    name: String,
    /// Set while its thread sends (it ends when the engine it sends goes).
    running: Arc<AtomicBool>,
    _worker: Worker,
}

impl NdiOut {
    /// Sends what's on air at `size`, `fps` a second, as the NDI source `name`.
    pub(crate) fn start(engine: Weak<Shared>, name: &str, size: (u32, u32), fps: u32) -> Result<NdiOut, String> {
        let mut tx = Sender::new(name, fps)?;
        let (w, h) = (size.0.max(2), size.1.max(2));
        let running = Arc::new(AtomicBool::new(true));
        let alive = running.clone();
        let worker = spawn("ferrocast-ndi-out", move |stop| {
            let every = Duration::from_secs(1) / fps.clamp(1, 120);
            let mut next = Instant::now();
            while !stop.load(Ordering::Acquire) {
                let Some(shared) = engine.upgrade() else { break };
                let frame = shared.engine.snapshot().ok();
                drop(shared);
                if let Some(f) = frame {
                    tx.send(&f, w, h);
                }
                next += every;
                match next.checked_duration_since(Instant::now()) {
                    Some(rest) => std::thread::sleep(rest),
                    None => next = Instant::now(),
                }
            }
            alive.store(false, Ordering::Release);
        })?;
        Ok(NdiOut { name: name.to_string(), running, _worker: worker })
    }

    /// Whether it's still sending.
    pub fn active(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// The name it's sent under.
    pub fn name(&self) -> &str {
        &self.name
    }
}
