//! Remove background on our engine: for each such filter, a thread takes a small copy of its
//! source about 15 times a second, finds the person in it (Apple's Vision on a Mac, Robust Video
//! Matting on Windows) and hands the mask to the filter's own picture (`Effect::Cutout` keeps
//! the person by it). Until the first mask the picture shows as it is.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ferrocore::Picture;

use crate::own::Shared;

/// How often a new mask is asked for.
const EVERY: Duration = Duration::from_millis(66);

/// The small copy Vision is asked about: 320 wide, the height following the picture's shape.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn ask_size(w: u32, h: u32) -> (u32, u32) {
    let sw = 320.min(w.max(1));
    (sw, ((u64::from(h) * u64::from(sw)) / u64::from(w.max(1))).max(1) as u32)
}

/// What finds the person, kept between frames (RVM carries what it saw).
#[derive(Default)]
struct Finder {
    #[cfg(windows)]
    matte: Option<crate::ffi::matte::Matte>,
}

impl Finder {
    /// The size of the copy to ask about.
    fn size(&self, w: u32, h: u32) -> (u32, u32) {
        #[cfg(windows)]
        return (crate::ffi::matte::WIDTH, crate::ffi::matte::height_for(w, h));
        #[cfg(not(windows))]
        ask_size(w, h)
    }

    /// The person in an RGBA picture: one byte a pixel, and the mask's size. None until it can
    /// tell (Windows: the model is still on its way).
    fn person(&mut self, rgba: &[u8], w: u32, h: u32) -> Option<(Vec<u8>, u32, u32)> {
        #[cfg(target_os = "macos")]
        return crate::segment::person_mask(rgba, w, h).map(|m| (m.px, m.w, m.h));
        #[cfg(windows)]
        {
            if !crate::ffi::matte::ready() {
                return None;
            }
            let m = self.matte.get_or_insert_with(crate::ffi::matte::Matte::new);
            match m.mask(rgba, w, h) {
                Ok(px) => Some((px, w, h)),
                Err(e) => {
                    tracing::debug!(error = %e, "Remove background didn't find the person in a frame");
                    None
                }
            }
        }
    }
}

/// One Remove background filter at work. Dropping it stops its thread.
#[derive(Debug)]
pub(crate) struct Cutout {
    /// The engine it works for (a new one needs a new worker).
    engine: Weak<Shared>,
    stop: Arc<AtomicBool>,
    /// The mask pictures it feeds: one worker serves every filter on its source.
    masks: Arc<Mutex<Vec<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl Cutout {
    /// Finds the person in `source` and hands masks to the pictures called `masks`.
    pub(crate) fn start(engine: Weak<Shared>, source: String, masks: Vec<String>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let masks = Arc::new(Mutex::new(masks));
        let thread = {
            let (stop, engine, masks) = (stop.clone(), engine.clone(), masks.clone());
            std::thread::Builder::new()
                .name("ferrocast-cutout".into())
                .spawn(move || run(&engine, &source, &masks, &stop))
                .ok()
        };
        Self { engine, stop, masks, thread }
    }

    /// Changes which pictures it feeds (the same source, other filters or items).
    pub(crate) fn set_masks(&self, masks: Vec<String>) {
        if let Ok(mut m) = self.masks.lock() {
            *m = masks;
        }
    }

    /// Whether it works for this engine.
    pub(crate) fn serves(&self, e: &Arc<Shared>) -> bool {
        Weak::ptr_eq(&self.engine, &Arc::downgrade(e))
    }
}

impl Drop for Cutout {
    fn drop(&mut self) {
        // Not joined: a pass of Vision can take 200 ms, and this runs on the UI thread (a delete,
        // an undo). The thread sees `stop` after its pass and ends; it holds only a Weak engine.
        self.stop.store(true, Ordering::Release);
        drop(self.thread.take());
    }
}

fn run(engine: &Weak<Shared>, source: &str, masks: &Mutex<Vec<String>>, stop: &AtomicBool) {
    // The mask picture each name last went to: a new one (the collection opened again) starts
    // clear, so it's given "all person" at once.
    let mut fed_ids = std::collections::HashMap::new();
    let mut finder = Finder::default();
    while !stop.load(Ordering::Acquire) {
        let started = Instant::now();
        let Some(shared) = engine.upgrade() else { return };
        let e = &shared.engine;
        let names = masks.lock().map(|m| m.clone()).unwrap_or_default();
        fed_ids.retain(|n, _| names.contains(n));
        if let Some((src, Some((w, h)))) = e.source_named(source) {
            let targets: Vec<_> = names
                .iter()
                .filter_map(|n| {
                    let (mid, _) = e.source_named(n)?;
                    Some((n, mid, e.fed(mid)?))
                })
                .collect();
            for (n, mid, feed) in &targets {
                if fed_ids.get(*n) != Some(mid) {
                    fed_ids.insert((*n).clone(), *mid);
                    feed.picture(Picture { width: 1, height: 1, rgba: vec![255; 4], premultiplied: false });
                }
            }
            if !targets.is_empty() {
                let (sw, sh) = finder.size(w, h);
                if let Some(rgba) = e.snapshot_source_at(src, (sw, sh))
                    && let Some((px, mw, mh)) = finder.person(&rgba, sw, sh)
                {
                    // One look at the source, handed to each filter's picture.
                    let rgba: Vec<u8> = px.iter().flat_map(|v| [*v, *v, *v, 255]).collect();
                    for (_, _, feed) in &targets {
                        feed.picture(Picture { width: mw, height: mh, rgba: rgba.clone(), premultiplied: false });
                    }
                }
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
