//! "Everything on" soak of the real engine (render loop, mixer, audio thread, an output).
//!
//! The scene set uses together: one camera in three scenes with item-own filters that differ,
//! a Video Delay (with sound) on the camera in one of them, a mic on push-to-talk with its key
//! going down and up, a filter switched by name (the effects list swapped), items hidden and
//! shown, a scene switch every few seconds and a stream output (no network: tags are counted
//! and dropped). The engine runs on the wall clock, so the default run is short
//! (`FERROCAST_SOAK_SECS`, 15 s); the long one is `#[ignore]`d (default 300 s):
//! `cargo test -p ferrocore --release --test soak_engine -- --ignored --nocapture`.
//!
//! Asserts: no allocation or free on the audio thread after its first second (during the churn too:
//! scene switches, effects swaps, hide/show, the talk key). The first second is set-up: a source
//! appearing registers on the audio thread and allocates there (`FERROCAST_SOAK_TRACE=1` prints where), frame times inside a budget, almost no skipped frames, memory (RSS) and
//! thread count not growing, no panics (`Stats::recovered` stays 0), a clean stop.
//! Skipped (and passes) where there is no graphics card.

#![allow(unsafe_code, clippy::unwrap_used, clippy::expect_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferrocore::audio::AudioConfig;
use ferrocore::engine::{Engine, EngineConfig, OutputKind, SceneItem, SourceKind, now_ns};
use ferrocore::mix::SourceSettings;
use ferrocore::video::{PixelFormat, RateControl, VideoCodec, VideoConfig};
use ferrocore::{Effect, Picture, Placement};

/// Counts allocations made by the thread called `ferrocast-audio`, and all others apart.
struct Counting;
static AUDIO_ALLOCS: AtomicUsize = AtomicUsize::new(0);
/// `FERROCAST_SOAK_TRACE=1`: where the audio thread allocates, by call chain.
static TRACE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static AUDIO_FREES: AtomicUsize = AtomicUsize::new(0);
static WARM: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static SITES: Mutex<std::collections::BTreeMap<String, u32>> = Mutex::new(std::collections::BTreeMap::new());
thread_local! {
    static BUSY: Cell<bool> = const { Cell::new(false) };
    /// 0: not looked up yet, 1: the audio thread, 2: another one (looked up once, while the
    /// thread's own data is alive: asking its name at the very end of its life panics).
    static WHO: Cell<u8> = const { Cell::new(0) };
}

fn is_audio_thread() -> bool {
    WHO.try_with(|w| {
        if w.get() == 0 {
            w.set(if std::thread::current().name() == Some("ferrocast-audio") { 1 } else { 2 });
        }
        w.get() == 1
    })
    .unwrap_or(false)
}

fn note() {
    note_kind(&AUDIO_ALLOCS);
}

fn note_kind(counter: &AtomicUsize) {
    // Looking up the thread's name must not count (or recurse into) itself.
    let _ = BUSY.try_with(|b| {
        if !b.replace(true) {
            if is_audio_thread() {
                counter.fetch_add(1, Ordering::Relaxed);
                if TRACE.load(Ordering::Relaxed) || WARM.load(Ordering::Relaxed) {
                    let bt = std::backtrace::Backtrace::force_capture().to_string();
                    let frames: Vec<&str> = bt
                        .lines()
                        .map(str::trim)
                        .filter(|l| !l.starts_with("at "))
                        .map(|l| l.split_once(": ").map_or(l, |x| x.1))
                        .skip_while(|l| !l.starts_with("__rust_"))
                        .skip_while(|l| l.starts_with("__rust_"))
                        .filter(|l| {
                            ![
                                "alloc::",
                                "hashbrown",
                                "core::",
                                "std::",
                                "soak_engine",
                                "Counting",
                                "<alloc",
                                "<core",
                                "<std",
                            ]
                            .iter()
                            .any(|x| l.starts_with(x))
                        })
                        .map(|l| l.split('<').next().unwrap_or(l))
                        .take(9)
                        .collect();
                    let warm = if WARM.load(Ordering::Relaxed) { "after 1s" } else { "first 1s" };
                    let kind = if std::ptr::eq(counter, &AUDIO_ALLOCS) { "alloc" } else { "free" };
                    let frames = [kind, warm].into_iter().chain(frames).collect::<Vec<_>>();
                    *SITES.lock().unwrap_or_else(|p| p.into_inner()).entry(frames.join(" <- ")).or_insert(0) += 1;
                }
            }
            b.set(false);
        }
    });
}

// SAFETY: everything is passed to the system allocator unchanged; `note` only adds to a counter.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        note();
        // SAFETY: the caller's layout.
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        note();
        // SAFETY: as above.
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        note_kind(&AUDIO_FREES);
        // SAFETY: a pointer and layout from `alloc`.
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        note();
        // SAFETY: as above.
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

fn rss_kb() -> u64 {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output();
    out.ok().and_then(|o| String::from_utf8(o.stdout).ok()).and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

fn threads() -> usize {
    #[cfg(target_os = "linux")]
    return std::fs::read_dir("/proc/self/task").map_or(0, Iterator::count);
    #[cfg(not(target_os = "linux"))]
    {
        let out = std::process::Command::new("ps").args(["-M", "-p", &std::process::id().to_string()]).output();
        out.map_or(0, |o| String::from_utf8_lossy(&o.stdout).lines().count().saturating_sub(1))
    }
}

fn encodes_here() -> bool {
    cfg!(target_os = "macos") || ferrocore::ffenc::h264_encoder().is_some()
}

fn engine() -> Option<Engine> {
    let video = VideoConfig {
        codec: VideoCodec::H264,
        input: PixelFormat::Nv12,
        width: 640,
        height: 360,
        fps_num: 30,
        fps_den: 1,
        bitrate_kbps: 2000,
        rate: RateControl::Cbr,
        keyframe_secs: 1,
        software: false,
        profile: ferrocore::video::H264Profile::High,
        bframes: false,
        preset: None,
        tune: None,
    };
    let audio = AudioConfig { sample_rate: 48_000, channels: 2, bitrate_kbps: 128 };
    Engine::new(EngineConfig { width: 640, height: 360, fps: 30, ..Default::default() }, video, audio).ok()
}

const CAM: u64 = 1;
const MIC: u64 = 2;
const CARD: u64 = 3;

fn cam_item(effects: Vec<Effect>, visible: bool) -> SceneItem {
    let mut it = SceneItem::new(CAM, Placement::default());
    it.effects = effects;
    it.visible = visible;
    it
}

/// The scenes, as the soak edits them: `round` flips what is hidden and which filter is on.
fn set_scenes(e: &Engine, round: u64) {
    let on = round.is_multiple_of(2);
    let round_fx = |radius: f32| if on { vec![Effect::RoundedCorners { radius }] } else { Vec::new() };
    // A: the camera with a Video Delay (sound too) and its own filters.
    let mut a = round_fx(12.0);
    a.push(Effect::Delay { ms: 300, audio: true });
    e.set_scene("A", vec![cam_item(a, true), SceneItem::new(MIC, Placement::default())]);
    // B: the same camera with another list of its own, hidden every third round.
    e.set_scene(
        "B",
        vec![cam_item(round_fx(40.0), !round.is_multiple_of(3)), SceneItem::new(CARD, Placement::default())],
    );
    // C: the camera plain, over a card that is hidden and shown.
    let mut card = SceneItem::new(CARD, Placement { x: 200.0, ..Placement::default() });
    card.visible = on;
    e.set_scene("C", vec![cam_item(Vec::new(), true), card]);
}

fn run(secs: u64) {
    if !encodes_here() {
        eprintln!("skipped: no H.264 encoder here");
        return;
    }
    TRACE.store(std::env::var_os("FERROCAST_SOAK_TRACE").is_some(), Ordering::Relaxed);
    let Some(e) = engine() else {
        eprintln!("skipped: no engine (no graphics card)");
        return;
    };
    e.set_source(CAM, SourceKind::Fed { width: 320, height: 180 }).unwrap();
    e.set_source(MIC, SourceKind::Fed { width: 16, height: 16 }).unwrap();
    e.set_source(CARD, SourceKind::Color { width: 100, height: 100, rgba: [200, 30, 30, 255] }).unwrap();
    e.set_sound(MIC, SourceSettings { push_to_talk: Some(150), ..Default::default() });
    e.set_sound(CAM, SourceSettings::default());
    set_scenes(&e, 0);
    e.go("A").unwrap();
    let (cam, mic) = (e.fed(CAM).unwrap(), e.fed(MIC).unwrap());

    let tags = Arc::new(Mutex::new((0u64, 0u64)));
    let sink = {
        let tags = tags.clone();
        move |t: ferrocore::pipeline::Tag| {
            let mut g = tags.lock().unwrap_or_else(|p| p.into_inner());
            match t {
                ferrocore::pipeline::Tag::Video { .. } => g.0 += 1,
                ferrocore::pipeline::Tag::Audio { .. } => g.1 += 1,
            }
        }
    };
    let out = e.start_output(OutputKind::Stream(Box::new(sink))).unwrap();

    let tone: Vec<f32> = (0..960)
        .flat_map(|i| {
            let s = 0.2 * (std::f32::consts::TAU * 220.0 * i as f32 / 48_000.0).sin();
            [s, s]
        })
        .collect();
    let scenes = ["A", "B", "C"];
    let started = Instant::now();
    let warm = Duration::from_secs(secs.min(10) / 3 + 2);
    let churn_end = Duration::from_secs(secs);
    let (mut rss_mark, mut threads_mark, mut spikes, mut samples) = (0u64, 0usize, 0u64, 0u64);
    let mut worst = 0.0f32;
    let mut idle_from: Option<(Instant, usize)> = None;
    let (mut warm_allocs, mut warm_frees) = (0usize, 0usize);
    let mut guest: Option<(u64, ferrocore::engine::Feed)> = None;
    let mut tick = 0u64;
    let mut base = None;
    while started.elapsed() < churn_end + Duration::from_secs(4) {
        let now = started.elapsed();
        let churn = now < churn_end;
        // Sources deliver at 30 fps / 20 ms pieces whatever the engine is doing.
        if tick.is_multiple_of(2) {
            cam.picture(Picture::solid(320, 180, [(tick % 250) as u8, 90, 160, 255]));
        }
        cam.sound(tone.clone(), 48_000, now_ns());
        mic.sound(tone.clone(), 48_000, now_ns());
        if churn {
            // 500 ms: the talk key; 1 s: hide/show + the filter by name; 3 s: scene switch.
            if tick.is_multiple_of(25) {
                e.push_key(MIC, true, (tick / 25).is_multiple_of(2));
            }
            // A source appears every 2 s (a guest, a browser source...) and goes 1 s later, its
            // sound flowing: registering it and dropping it mustn't allocate or free there.
            if tick % 100 == 10 {
                let id = 100 + tick / 100;
                e.set_source(id, SourceKind::Fed { width: 16, height: 16 }).unwrap();
                guest = Some((id, e.fed(id).unwrap()));
            }
            if let Some((_, f)) = &guest {
                f.sound(tone.clone(), 48_000, now_ns());
            }
            if tick % 100 == 60
                && let Some((id, _)) = guest.take()
            {
                e.remove_source(id);
            }
            if tick.is_multiple_of(50) {
                set_scenes(&e, tick / 50);
            }
            if tick % 150 == 75 {
                e.go(scenes[((tick / 150) % 3) as usize]).unwrap();
            }
        } else if idle_from.is_none() {
            // Everything settles: from here only sound and pictures flow.
            e.push_key(MIC, true, true);
            idle_from = Some((Instant::now() + Duration::from_secs(2), AUDIO_ALLOCS.load(Ordering::SeqCst)));
        }
        if let Some((at, _)) = idle_from.as_mut()
            && base.is_none()
            && Instant::now() >= *at
        {
            base = Some(AUDIO_ALLOCS.load(Ordering::SeqCst));
        }
        let s = e.stats();
        if now > warm {
            samples += 1;
            if s.frame_ms > 60.0 {
                spikes += 1;
            }
            worst = worst.max(s.frame_ms);
        }
        if tick == 50 {
            WARM.store(true, Ordering::Relaxed);
            warm_allocs = AUDIO_ALLOCS.load(Ordering::SeqCst);
            warm_frees = AUDIO_FREES.load(Ordering::SeqCst);
        }
        if rss_mark == 0 && now > warm {
            rss_mark = rss_kb();
            threads_mark = threads();
        }
        std::thread::sleep(Duration::from_millis(20));
        tick += 1;
    }
    let idle_allocs = base.map_or(0, |b| AUDIO_ALLOCS.load(Ordering::SeqCst) - b);
    let churn_allocs = idle_from.map_or(0, |(_, a)| a);
    let steady_allocs = AUDIO_ALLOCS.load(Ordering::SeqCst) - warm_allocs;
    let steady_frees = AUDIO_FREES.load(Ordering::SeqCst) - warm_frees;
    let (rss_end, threads_end) = (rss_kb(), threads());
    let st = e.stats();
    let (video_tags, audio_tags) = *tags.lock().unwrap();
    eprintln!(
        "soak {secs}s: {} frames, {} skipped, worst {worst:.1} ms, {spikes}/{samples} samples over 60 ms, \
         audio-thread allocations: {churn_allocs} from start to end of churn (warm-up and control commands), {idle_allocs} idle; rss {rss_mark} -> {rss_end} kB; audio-thread allocations after the first second (churn and idle) {steady_allocs}, frees {steady_frees}; \
         threads {threads_mark} -> {threads_end}; tags {video_tags} video {audio_tags} audio; recovered {}",
        st.frames, st.skipped, st.recovered
    );

    for (site, n) in SITES.lock().unwrap_or_else(|p| p.into_inner()).iter() {
        eprintln!("audio-thread allocation x{n} : {site}");
    }
    assert_eq!(st.recovered, 0, "a fault was caught in the render loop");
    assert!(video_tags > secs && audio_tags > secs, "the output kept getting pictures and sound");
    let share = st.skipped as f64 * 100.0 / (st.frames + st.skipped).max(1) as f64;
    assert!(share < 5.0, "{share:.1}% of frame times skipped");
    assert!(spikes * 50 <= samples, "{spikes} of {samples} frames took over 60 ms (worst {worst} ms)");
    assert_eq!(
        steady_allocs, 0,
        "the audio thread allocated {steady_allocs} times after its first second (scene switches, effects, hide/show, talk key)"
    );
    // The one thing left: std's unbounded command channel frees a block of its own for about
    // every 31 commands it has carried (and allocates one on the sending side; the audio thread
    // sends only drop jobs). A bounded channel would remove it, at the cost of senders waiting.
    assert!(
        steady_frees <= 2 + secs as usize / 5,
        "the audio thread freed {steady_frees} buffers after its first second"
    );
    assert_eq!(idle_allocs, 0, "the audio thread allocated {idle_allocs} times while idle");
    assert!(rss_end < rss_mark + 150_000, "memory grew from {rss_mark} to {rss_end} kB");
    assert!(threads_end <= threads_mark + 2, "threads grew from {threads_mark} to {threads_end}");

    // Clean stop: the output ends and the engine drops within a few seconds, taking its threads.
    let before = threads();
    e.stop_output(out).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(e);
        let _ = tx.send(());
    });
    assert!(rx.recv_timeout(Duration::from_secs(15)).is_ok(), "the engine took over 15 s to stop");
    std::thread::sleep(Duration::from_secs(1));
    let after = threads();
    assert!(after + 3 <= before, "threads left running after the engine stopped: {before} -> {after}");
}

#[test]
fn everything_on_short_soak() {
    run(std::env::var("FERROCAST_SOAK_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(15));
}

#[test]
#[ignore = "five real minutes; run by hand with --ignored"]
fn everything_on_long_soak() {
    run(std::env::var("FERROCAST_SOAK_LONG_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(300));
}
