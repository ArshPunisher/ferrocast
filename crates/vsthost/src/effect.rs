//! The engine's side: a VST filter as one of ferrocore's sound plugins. Its helper starts on a
//! thread of its own (never in the mixer's lock), and again after a crash or a hang, waiting
//! longer each time; meanwhile the sound passes as it is.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use ferrocore::afx::SoundPlugin;

use crate::client::{Helper, helper_exe};
use crate::proto;

/// The most a block may be (the mixer's are 1024).
const MAX_BLOCK: usize = 8192;

/// One VST filter's plugin, shared by the mixer (blocks) and the app (its window, its state).
#[derive(Debug)]
pub struct Live {
    pub slot: u32,
    rate: u32,
    helper: Mutex<Option<Helper>>,
    starting: AtomicBool,
    tries: AtomicU32,
    next_try: Mutex<Instant>,
    latency: AtomicU32,
    /// Why it isn't running, when it isn't.
    problem: Mutex<Option<String>>,
}

static LIVE: Mutex<Vec<Weak<Live>>> = Mutex::new(Vec::new());

/// Makes ferrocore's VST filters run through the helper (when the helper is here).
pub fn install() {
    ferrocore::afx::set_plugin_maker(make);
}

fn make(slot: u32, rate: u32) -> Option<Box<dyn SoundPlugin>> {
    // Only where it runs locked down.
    crate::available().ok()?;
    let live = Arc::new(Live {
        slot,
        rate,
        helper: Mutex::new(None),
        starting: AtomicBool::new(false),
        tries: AtomicU32::new(0),
        next_try: Mutex::new(Instant::now()),
        latency: AtomicU32::new(0),
        problem: Mutex::new(None),
    });
    if let Ok(mut l) = LIVE.lock() {
        l.retain(|w| w.strong_count() > 0);
        l.push(Arc::downgrade(&live));
    }
    live.start_soon();
    Some(Box::new(VstEffect { live }))
}

/// The running plugin in `slot`, if there is one: the newest (while the mixer rebuilds its
/// filters, the one it's letting go can still be there).
pub fn live(slot: u32) -> Option<Arc<Live>> {
    LIVE.lock().ok()?.iter().rev().filter_map(Weak::upgrade).find(|l| l.slot == slot)
}

impl Live {
    /// Starts its helper on its own thread, unless it's starting or waiting before trying again.
    fn start_soon(self: &Arc<Self>) {
        if self.next_try.lock().is_ok_and(|t| Instant::now() < *t) || self.starting.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.clone();
        let spawned = std::thread::Builder::new().name("ferrocast-vst-start".into()).spawn(move || {
            let made = me.start_now();
            match made {
                Ok(h) => {
                    me.latency.store(h.latency, Ordering::Release);
                    me.tries.store(0, Ordering::Release);
                    if let Ok(mut p) = me.problem.lock() {
                        *p = None;
                    }
                    tracing::info!(slot = me.slot, plugin = %h.name, "a VST plugin is running");
                    if let Ok(mut g) = me.helper.lock() {
                        *g = Some(h);
                    }
                }
                Err(e) => {
                    let n = me.tries.fetch_add(1, Ordering::AcqRel) + 1;
                    // 2, 4, 8… seconds, at most a minute.
                    let wait = Duration::from_secs((1u64 << n.min(6)).min(60));
                    if let Ok(mut t) = me.next_try.lock() {
                        *t = Instant::now() + wait;
                    }
                    tracing::warn!(slot = me.slot, error = %e, ?wait, "a VST plugin didn't start");
                    if let Ok(mut p) = me.problem.lock() {
                        *p = Some(e);
                    }
                }
            }
            me.starting.store(false, Ordering::Release);
        });
        if spawned.is_err() {
            self.starting.store(false, Ordering::Release);
        }
    }

    fn start_now(&self) -> Result<Helper, String> {
        let spec = ferrocore::afx::plugin_spec(self.slot).ok_or("this filter has no plugin")?;
        let exe = helper_exe().ok_or("the VST helper isn't in this build")?;
        let state = proto::state_of(&spec.state);
        let state = state.as_ref().map(|(c, k)| (c.as_slice(), k.as_slice()));
        Helper::start(&exe, Path::new(&spec.path), &spec.cid, self.rate, MAX_BLOCK, state)
    }

    /// Opens or closes the plugin's own window.
    pub fn window(&self, open: bool) -> Result<(), String> {
        let mut g = self.helper.lock().map_err(|_| "busy")?;
        let h = g.as_mut().ok_or_else(|| self.problem().unwrap_or_else(|| "it's still starting".into()))?;
        h.window(open)
    }

    /// Its state now (component and controller, as saved in the filter), kept for its next
    /// start too.
    pub fn state(&self) -> Option<Vec<u8>> {
        let mut g = self.helper.lock().ok()?;
        let (c, k) = g.as_mut()?.state()?;
        let b = proto::state_bytes(&c, &k);
        ferrocore::afx::set_plugin_state(self.slot, b.clone());
        Some(b)
    }

    /// Whether its window changed something since its state was last read.
    pub fn changed(&self) -> bool {
        self.helper.lock().ok().and_then(|g| g.as_ref().map(|h| h.changed)).unwrap_or(false)
    }

    /// Why it isn't running (None: it is, or it's starting).
    pub fn problem(&self) -> Option<String> {
        let dead = self.helper.lock().ok().and_then(|g| g.as_ref().and_then(|h| h.dead.clone()));
        dead.or_else(|| self.problem.lock().ok().and_then(|p| p.clone()))
    }

    /// Running now.
    pub fn running(&self) -> bool {
        self.helper.lock().ok().is_some_and(|g| g.as_ref().is_some_and(|h| h.dead.is_none()))
    }
}

/// A VST filter as the mixer sees it.
#[derive(Debug)]
pub struct VstEffect {
    live: Arc<Live>,
}

impl SoundPlugin for VstEffect {
    fn process(&mut self, ch: [&mut [f32]; 2]) -> bool {
        let [l, r] = ch;
        // The app is reading its state or opening its window: this block passes as it is.
        let Ok(mut g) = self.live.helper.try_lock() else { return false };
        match g.as_mut() {
            Some(h) if h.dead.is_none() => {
                let ok = h.process(l, r);
                if let Some(why) = &h.dead {
                    tracing::warn!(slot = self.live.slot, "{why}: it starts again");
                }
                ok
            }
            Some(_) => {
                // Gone: let go of it, and start it again (after a wait if it keeps failing).
                let n = self.live.tries.fetch_add(1, Ordering::AcqRel) + 1;
                if let Ok(mut t) = self.live.next_try.lock() {
                    *t = Instant::now() + Duration::from_secs((1u64 << n.min(6)).min(60));
                }
                if let Some(old) = g.take()
                    && let Ok(mut p) = self.live.problem.lock()
                {
                    *p = old.dead.clone();
                }
                drop(g);
                self.live.start_soon();
                false
            }
            None => {
                drop(g);
                self.live.start_soon();
                false
            }
        }
    }

    fn latency_frames(&self) -> usize {
        self.live.latency.load(Ordering::Acquire) as usize
    }
}
