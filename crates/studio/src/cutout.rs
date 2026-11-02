//! Remove background on our engine: for each such filter, a thread takes a small copy of its
//! source about 15 times a second, asks Vision for the person in it, and hands the mask to the
//! filter's own picture (`Effect::Cutout` keeps the person by it). Until the first mask the
//! picture shows as it is.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ferrocore::Picture;

use crate::own::Shared;

/// How often a new mask is asked for.
const EVERY: Duration = Duration::from_millis(66);

/// The small copy Vision is asked about: 320 wide, the height following the picture's shape.
fn ask_size(w: u32, h: u32) -> (u32, u32) {
    let sw = 320.min(w.max(1));
    (sw, ((u64::from(h) * u64::from(sw)) / u64::from(w.max(1))).max(1) as u32)
}

/// One Remove background filter at work. Dropping it stops its thread.
#[derive(Debug)]
pub(crate) struct Cutout {
    /// The engine it works for (a new one needs a new worker).
    engine: Weak<Shared>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Cutout {
    /// Finds the person in `source` and hands masks to the picture called `mask`.
    pub(crate) fn start(engine: Weak<Shared>, source: String, mask: String) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (stop, engine) = (stop.clone(), engine.clone());
            std::thread::Builder::new()
                .name("ferrocast-cutout".into())
                .spawn(move || run(&engine, &source, &mask, &stop))
                .ok()
        };
        Self { engine, stop, thread }
    }

    /// Whether it works for this engine.
    pub(crate) fn serves(&self, e: &Arc<Shared>) -> bool {
        Weak::ptr_eq(&self.engine, &Arc::downgrade(e))
    }
}

impl Drop for Cutout {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(engine: &Weak<Shared>, source: &str, mask: &str, stop: &AtomicBool) {
    // The mask picture the last mask went to: a new one (the collection opened again) starts
    // clear, so it's given "all person" at once.
    let mut fed_id = None;
    while !stop.load(Ordering::Acquire) {
        let started = Instant::now();
        let Some(shared) = engine.upgrade() else { return };
        let e = &shared.engine;
        if let (Some((src, Some((w, h)))), Some((mid, _))) = (e.source_named(source), e.source_named(mask))
            && let Some(feed) = e.fed(mid)
        {
            if fed_id != Some(mid) {
                fed_id = Some(mid);
                feed.picture(Picture { width: 1, height: 1, rgba: vec![255; 4], premultiplied: false });
            }
            let (sw, sh) = ask_size(w, h);
            if let Some(rgba) = e.snapshot_source_at(src, (sw, sh))
                && let Some(m) = crate::segment::person_mask(&rgba, sw, sh)
            {
                let rgba = m.px.iter().flat_map(|v| [*v, *v, *v, 255]).collect();
                feed.picture(Picture { width: m.w, height: m.h, rgba, premultiplied: false });
            }
        }
        drop(shared);
        let rest = EVERY.saturating_sub(started.elapsed());
        std::thread::sleep(rest.max(Duration::from_millis(5)));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn asks_about_a_small_copy_of_the_same_shape() {
        assert_eq!(super::ask_size(1920, 1080), (320, 180));
        assert_eq!(super::ask_size(200, 100), (200, 100));
        assert_eq!(super::ask_size(0, 0), (1, 1));
    }
}
