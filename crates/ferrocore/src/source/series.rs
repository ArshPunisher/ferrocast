//! Files one after another, each on a worker thread of its own so the frame never waits on a
//! file: pictures (OBS's Image Slide Show, with a fade between them) and video or sound files
//! (OBS's VLC Video Source playlist). Each hands what it shows to a fed source.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::Picture;
use crate::engine::{Feed, now_ns};
use crate::source::image::ImageSource;

/// How a slideshow goes.
#[derive(Debug, Clone, PartialEq)]
pub struct Slides {
    pub files: Vec<PathBuf>,
    /// How long each picture shows.
    pub every_ms: u64,
    /// The fade between two pictures (0: a cut).
    pub fade_ms: u64,
    /// From the first again after the last (else the last stays).
    pub looping: bool,
    /// In a shuffled order (a new one each time round).
    pub random: bool,
}

/// A playlist of video or sound files.
#[derive(Debug, Clone, PartialEq)]
pub struct Playlist {
    pub files: Vec<PathBuf>,
    pub looping: bool,
    pub random: bool,
}

/// A worker thread; dropping it stops it.
#[derive(Debug)]
pub(crate) struct Worker {
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

fn spawn(name: &str, f: impl FnOnce(&AtomicBool) + Send + 'static) -> Worker {
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let stop = stop.clone();
        std::thread::Builder::new().name(name.into()).spawn(move || f(&stop)).ok()
    };
    Worker { stop, thread }
}

/// Waits `d`, in short steps → false when stopped meanwhile.
fn wait(stop: &AtomicBool, d: Duration) -> bool {
    let step = Duration::from_millis(20);
    let mut left = d;
    while !left.is_zero() {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        let s = left.min(step);
        std::thread::sleep(s);
        left -= s;
    }
    !stop.load(Ordering::Acquire)
}

/// The order to show `n` files in: as listed, or shuffled (xorshift, seeded by the clock).
fn order(n: usize, random: bool, seed: &mut u64) -> Vec<usize> {
    let mut o: Vec<usize> = (0..n).collect();
    if random {
        for i in (1..n).rev() {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 7;
            *seed ^= *seed << 17;
            o.swap(i, (*seed % (i as u64 + 1)) as usize);
        }
    }
    o
}

/// `a` turning into `b` (0 to 1), when they're the same size and kind.
fn blend(a: &Picture, b: &Picture, t: f32) -> Option<Picture> {
    if (a.width, a.height, a.premultiplied) != (b.width, b.height, b.premultiplied) || a.rgba.len() != b.rgba.len() {
        return None;
    }
    let rgba = a
        .rgba
        .iter()
        .zip(&b.rgba)
        .map(|(x, y)| (f32::from(*x) * (1.0 - t) + f32::from(*y) * t).round() as u8)
        .collect();
    Some(Picture { width: a.width, height: a.height, rgba, premultiplied: a.premultiplied })
}

/// Shows the pictures into `feed`, one after another.
pub(crate) fn slideshow(s: Slides, feed: Feed) -> Worker {
    spawn("ferrocast-slideshow", move |stop| {
        let mut seed = now_ns() | 1;
        let mut shown: Option<Picture> = None;
        loop {
            let list = order(s.files.len(), s.random, &mut seed);
            for i in list {
                let next = match ImageSource::open(&s.files[i]) {
                    Ok(img) => img.frame_at(Duration::ZERO).clone(),
                    Err(e) => {
                        tracing::warn!(error = %e, "a slideshow picture didn't open: skipped");
                        if !wait(stop, Duration::from_millis(200)) {
                            return;
                        }
                        continue;
                    }
                };
                // A fade from the one before, about 30 steps a second.
                if let Some(prev) = &shown
                    && s.fade_ms > 0
                {
                    let steps = (s.fade_ms / 33).max(1);
                    for k in 1..steps {
                        let Some(p) = blend(prev, &next, k as f32 / steps as f32) else { break };
                        feed.picture(p);
                        if !wait(stop, Duration::from_millis(33)) {
                            return;
                        }
                    }
                }
                feed.picture(next.clone());
                shown = Some(next);
                // One picture alone just stays.
                if s.files.len() == 1 || !wait(stop, Duration::from_millis(s.every_ms.max(100))) {
                    return;
                }
            }
            if !s.looping || (shown.is_none() && !wait(stop, Duration::from_secs(5))) {
                return;
            }
        }
    })
}

/// Plays the files into `feed`, one after another: pictures and sound.
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
pub(crate) fn playlist(p: Playlist, feed: Feed) -> Worker {
    spawn("ferrocast-playlist", move |stop| {
        let mut seed = now_ns() | 1;
        loop {
            let mut played = false;
            for i in order(p.files.len(), p.random, &mut seed) {
                let m = match crate::media::Media::open(&p.files[i], false) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(error = %e, "a playlist file didn't open: skipped");
                        if !wait(stop, Duration::from_millis(200)) {
                            return;
                        }
                        continue;
                    }
                };
                played = true;
                while !m.ended() {
                    if let Some(pic) = m.frame_at(now_ns()) {
                        feed.picture(pic);
                    }
                    for (pcm, t) in m.take_sound() {
                        feed.sound(pcm, 48_000, t);
                    }
                    if !wait(stop, Duration::from_millis(5)) {
                        return;
                    }
                }
            }
            // Round again; a list where nothing opens waits before trying again.
            if !p.looping || (!played && !wait(stop, Duration::from_secs(5))) {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shuffles_keep_every_file_once() {
        let mut seed = 12345;
        let o = order(10, true, &mut seed);
        let mut sorted = o.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..10).collect::<Vec<_>>());
        assert_ne!(o, (0..10).collect::<Vec<_>>(), "shuffled");
        assert_eq!(order(3, false, &mut seed), vec![0, 1, 2]);
        assert!(order(0, true, &mut seed).is_empty());
    }

    #[test]
    fn a_fade_is_between_two_pictures_of_one_size() {
        let a = Picture::solid(2, 1, [0, 0, 0, 255]);
        let b = Picture::solid(2, 1, [200, 100, 50, 255]);
        let half = blend(&a, &b, 0.5).unwrap_or_else(|| panic!("same size"));
        assert_eq!(&half.rgba[..4], &[100, 50, 25, 255]);
        assert!(blend(&a, &Picture::solid(1, 1, [0, 0, 0, 255]), 0.5).is_none(), "a cut between sizes");
    }

    #[test]
    fn a_slideshow_shows_each_picture_in_turn() {
        let dir = std::env::temp_dir().join(format!("ferrocore-slides-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut files = Vec::new();
        for (name, c) in [("a.png", [255u8, 0, 0]), ("b.png", [0, 0, 255])] {
            let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([c[0], c[1], c[2], 255]));
            let path = dir.join(name);
            assert!(img.save(&path).is_ok());
            files.push(path);
        }
        let feed = Feed::default();
        let w = slideshow(Slides { files, every_ms: 300, fade_ms: 0, looping: true, random: false }, feed.clone());
        let mut seen = Vec::new();
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(25));
            if let Some(p) = feed.take_picture() {
                let c = [p.rgba[0], p.rgba[1], p.rgba[2]];
                if seen.last() != Some(&c) {
                    seen.push(c);
                }
            }
        }
        drop(w);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(seen.len() >= 3, "red, blue, red again: {seen:?}");
        assert_eq!(&seen[..3], &[[255, 0, 0], [0, 0, 255], [255, 0, 0]]);
    }
}
