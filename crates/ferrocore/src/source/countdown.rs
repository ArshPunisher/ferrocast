//! A countdown (the OBS plugins people add for "Starting in 4:59"): text that counts down for a
//! set time from when it's shown, or to a time of day, then shows an end message. Drawn on a
//! worker thread of its own, only when the text changes (once a second), and handed to a fed
//! source like a slideshow.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::engine::Feed;
use crate::source::series::{Worker, spawn, wait};
use crate::source::text::{TextRenderer, TextStyle};

/// What it counts to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CountTo {
    /// This long from when it's shown.
    For(Duration),
    /// This moment (milliseconds since 1970, UTC): a time of day, already turned into the next
    /// such moment by whoever made it.
    Until(u64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Countdown {
    pub to: CountTo,
    /// Before the time ("Starting in ").
    pub before: String,
    /// Shown when it reaches zero ("Starting now").
    pub end: String,
    pub style: TextStyle,
}

/// The time left as a clock: "4:59", or "1:04:59" from an hour up; whole seconds, rounded up
/// so it shows 0:01 until the very end.
pub fn clock(left: Duration) -> String {
    let s = left.as_millis().div_ceil(1000) as u64;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// What it says `left` before the end (None once it's over).
pub fn words(c: &Countdown, left: Option<Duration>) -> String {
    match left {
        Some(l) if !l.is_zero() => format!("{}{}", c.before, clock(l)),
        _ => c.end.clone(),
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// How long is left, from `started` (when it was shown).
fn left(c: &Countdown, started: Instant) -> Option<Duration> {
    match c.to {
        CountTo::For(d) => d.checked_sub(started.elapsed()),
        CountTo::Until(at) => at.checked_sub(now_ms()).map(Duration::from_millis),
    }
}

/// Counts down into `feed`, drawing again only when the words change, with the engine's
/// `text` renderer (its fonts are loaded once).
pub(crate) fn countdown(c: Countdown, feed: Feed, text: Arc<Mutex<TextRenderer>>) -> Worker {
    spawn("ferrocast-countdown", move |stop: &AtomicBool| {
        let started = Instant::now();
        let mut shown: Option<String> = None;
        loop {
            let now = words(&c, left(&c, started));
            if shown.as_deref() != Some(now.as_str()) {
                // Nothing to draw (an empty end message): a clear dot, not the last number.
                let p = if now.is_empty() {
                    crate::Picture::solid(1, 1, [0, 0, 0, 0])
                } else {
                    text.lock().unwrap_or_else(|e| e.into_inner()).render(&now, &c.style)
                };
                feed.picture(p);
                shown = Some(now);
            }
            if !wait(stop, Duration::from_millis(100)) {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cd(to: CountTo) -> Countdown {
        Countdown { to, before: "Starting in ".into(), end: "Starting now".into(), style: TextStyle::default() }
    }

    #[test]
    fn the_clock_reads_like_a_clock() {
        assert_eq!(clock(Duration::from_secs(299)), "4:59");
        assert_eq!(clock(Duration::from_secs(60)), "1:00");
        assert_eq!(clock(Duration::from_secs(3600 + 4 * 60 + 59)), "1:04:59");
        // Rounded up: 0:01 until the very end, never 0:00 while time is left.
        assert_eq!(clock(Duration::from_millis(400)), "0:01");
        assert_eq!(clock(Duration::from_millis(59_001)), "1:00");
    }

    #[test]
    fn it_says_the_time_then_the_end() {
        let c = cd(CountTo::For(Duration::from_secs(300)));
        assert_eq!(words(&c, Some(Duration::from_secs(299))), "Starting in 4:59");
        assert_eq!(words(&c, Some(Duration::ZERO)), "Starting now");
        assert_eq!(words(&c, None), "Starting now");
    }

    #[test]
    fn it_counts_from_when_it_is_shown_or_to_a_moment() {
        let c = cd(CountTo::For(Duration::from_secs(10)));
        let l = left(&c, Instant::now()).unwrap_or_default();
        assert!(l > Duration::from_secs(9) && l <= Duration::from_secs(10), "{l:?}");
        // Already over.
        assert_eq!(left(&c, Instant::now() - Duration::from_secs(11)), None);
        let soon = cd(CountTo::Until(now_ms() + 90_000));
        let l = left(&soon, Instant::now()).unwrap_or_default();
        assert!(l > Duration::from_secs(88) && l <= Duration::from_secs(90), "{l:?}");
        assert_eq!(left(&cd(CountTo::Until(now_ms().saturating_sub(5_000))), Instant::now()), None, "a moment passed");
    }

    /// The worker draws the first words at once and the end message when it's over.
    #[test]
    fn the_worker_draws_and_ends() {
        let fonts = Arc::new(Mutex::new(TextRenderer::new()));
        // The fonts read before the clock starts: a debug build reading a PC's many fonts took
        // longer than the 150 ms the first words had (10 Nov, Windows), which isn't what's tested.
        fonts.lock().unwrap_or_else(|e| e.into_inner()).render("warm", &TextStyle::default());
        let feed = Feed::default();
        let w = countdown(cd(CountTo::For(Duration::from_millis(300))), feed.clone(), fonts.clone());
        let next = || {
            let until = Instant::now() + Duration::from_secs(3);
            loop {
                if let Some(p) = feed.take_picture() {
                    return Some(p);
                }
                if Instant::now() > until {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let first = next().unwrap_or_else(|| panic!("a first picture"));
        assert!(first.width > 1, "words drawn");
        let last = next().unwrap_or_else(|| panic!("the end message"));
        assert!(last.width > 1);
        // Nothing new while nothing changes.
        std::thread::sleep(Duration::from_millis(300));
        assert!(feed.take_picture().is_none(), "drawn only when the words change");
        drop(w);
        // An empty end message: a clear dot, not the last number.
        let feed = Feed::default();
        let mut c = cd(CountTo::For(Duration::ZERO));
        c.end = String::new();
        let _w = countdown(c, feed.clone(), fonts);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(feed.take_picture().map(|p| (p.width, p.height)), Some((1, 1)));
    }
}
