//! Media from a network address (a guest's camera over WebRTC, described by an SDP file; an
//! RTMP, HLS or SRT address) for Ferrocast's own engine: FFmpeg reads it, as OBS's Media source
//! did, and hands its pictures (fitted into `SIZE`) and sound (48 kHz stereo) to a fed source.
//! When the stream drops, FFmpeg is started again after the source's reconnect delay.

use std::io::Read;
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use ferrocore::engine::{Feed, now_ns};
use serde_json::Value;

/// The size pictures are fitted into (their shape kept, the rest clear).
pub(crate) const SIZE: (u32, u32) = (1280, 720);
/// Sound read at a time (stereo frames).
const CHUNK: usize = 1024;

/// One network source being read. Dropping it stops FFmpeg.
pub(crate) struct NetMedia {
    feed: Arc<Mutex<Feed>>,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for NetMedia {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetMedia").finish_non_exhaustive()
    }
}

/// Which of a stream's picture and sound FFmpeg is asked for (it refuses to start when one is
/// asked for and the stream has none).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Parts {
    picture: bool,
    sound: bool,
}

const BOTH: Parts = Parts { picture: true, sound: true };

/// The FFmpeg arguments for a Media source's network settings (OBS's keys: `input`,
/// `input_format`, `ffmpeg_options`), its picture to stdout and its sound to `sound_to` (an
/// FFmpeg output address).
fn args(set: &Value, sound_to: &str, parts: Parts) -> Option<Vec<String>> {
    let input = set.get("input").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin"].map(String::from).to_vec();
    if let Some(f) = set.get("input_format").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        a.extend(["-f".into(), f.into()]);
    }
    // OBS's "FFmpeg options": `key=value` pairs, space apart, as input options.
    let opts = set.get("ffmpeg_options").and_then(Value::as_str).unwrap_or_default();
    // A live stream shows within a second, not after FFmpeg's default five of probing.
    if !opts.contains("analyzeduration=") {
        a.extend(["-analyzeduration".into(), "1000000".into()]);
    }
    for kv in opts.split_whitespace() {
        if let Some((k, v)) = kv.split_once('=')
            && !k.is_empty()
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            a.extend([format!("-{k}"), v.to_string()]);
        }
    }
    // Pictures decoded on the GPU when it can (FFmpeg falls back to the CPU by itself), unless
    // the options already say how.
    if parts.picture && ferrocore::ffsrc::hw_decode_allowed() && !opts.contains("hwaccel") {
        a.extend(["-hwaccel".into(), "auto".into()]);
    }
    a.extend(["-i".into(), input.into()]);
    let (w, h) = SIZE;
    let fit = format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black@0,format=rgba"
    );
    if parts.picture {
        a.extend(["-map", "0:v:0?", "-vf", &fit, "-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"].map(String::from));
    }
    if parts.sound {
        a.extend(["-map", "0:a:0?", "-f", "f32le", "-ar", "48000", "-ac", "2", "-y"].map(String::from));
        a.push(sound_to.to_string());
    }
    Some(a)
}

/// What to ask for next time, from FFmpeg's error output: an output it found nothing for is
/// left out (`out#0` is the picture when it was asked for, the sound otherwise).
fn parts_after(parts: Parts, errors: &str) -> Parts {
    let empty = |n: u8| errors.contains(&format!("[out#{n}/")) && errors.contains("does not contain any stream");
    let mut next = parts;
    if parts.picture && empty(0) {
        next.picture = false;
    }
    if (parts.picture && empty(1)) || (!parts.picture && empty(0)) {
        next.sound = false;
    }
    // Both gone means the stream had neither yet (still starting): ask for both again.
    if !next.picture && !next.sound { BOTH } else { next }
}

impl NetMedia {
    /// Starts reading the source with these settings into `feed`. Err when FFmpeg isn't here or
    /// the settings name nothing to read.
    pub(crate) fn start(set: &Value, feed: Feed) -> Result<Self, String> {
        let ffmpeg = ferrocore::ffenc::path()
            .ok_or("Media from a network address needs FFmpeg: pick x264 once in Settings and Ferrocast fetches it.")?;
        args(set, "", BOTH).ok_or("The media source has no address to read.")?;
        let set = set.clone();
        let delay = set.get("reconnect_delay_sec").and_then(Value::as_u64).unwrap_or(1).clamp(1, 60);
        let feed = Arc::new(Mutex::new(feed));
        let stop = Arc::new(AtomicBool::new(false));
        let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let thread = {
            let (feed, stop, child) = (feed.clone(), stop.clone(), child.clone());
            std::thread::Builder::new()
                .name("ferrocast-net-media".into())
                .spawn(move || run(&ffmpeg, &set, &feed, &stop, &child, Duration::from_secs(delay)))
                .map_err(|e| format!("The media source couldn't start: {e}"))?
        };
        Ok(Self { feed, stop, child, thread: Some(thread) })
    }

    /// Our engine's source changed (the collection was opened again): its new feed.
    pub(crate) fn set_feed(&self, feed: Feed) {
        *self.feed.lock().unwrap_or_else(|e| e.into_inner()) = feed;
    }
}

impl Drop for NetMedia {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(mut c) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// FFmpeg, started again after `delay` whenever it ends, until stopped.
fn run(
    ffmpeg: &std::path::Path,
    set: &Value,
    feed: &Arc<Mutex<Feed>>,
    stop: &AtomicBool,
    child: &Mutex<Option<Child>>,
    delay: Duration,
) {
    let mut parts = BOTH;
    while !stop.load(Ordering::Acquire) {
        // The picture comes through stdout, the sound through a connection FFmpeg makes to
        // this computer (every system has those; named pipes differ).
        let listener = match parts.sound.then(|| TcpListener::bind(("127.0.0.1", 0))).transpose() {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("a network media source couldn't open its sound connection: {e}");
                return;
            }
        };
        let sound_to = match listener.as_ref().map(TcpListener::local_addr).transpose() {
            Ok(Some(a)) => format!("tcp://{a}"),
            Ok(None) => String::new(),
            Err(e) => {
                tracing::warn!("a network media source couldn't open its sound connection: {e}");
                return;
            }
        };
        let Some(args) = args(set, &sound_to, parts) else { return };
        let spawned =
            Command::new(ffmpeg).args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
        let Ok(mut c) = spawned else {
            tracing::warn!("FFmpeg couldn't start for a network media source");
            return;
        };
        let out = c.stdout.take();
        // FFmpeg's errors (the first few kilobytes), read as they come so it never blocks on them.
        let errors = c.stderr.take().map(|e| {
            std::thread::spawn(move || {
                let mut text = String::new();
                let _ = e.take(8 * 1024).read_to_string(&mut text);
                text
            })
        });
        {
            // Stopped while FFmpeg was starting: `Drop` has looked for it already, so end it here.
            let mut slot = child.lock().unwrap_or_else(|e| e.into_inner());
            if stop.load(Ordering::Acquire) {
                let _ = c.kill();
                let _ = c.wait();
                return;
            }
            *slot = Some(c);
        }
        // Sound on its own thread (it waits for FFmpeg to connect).
        let done = Arc::new(AtomicBool::new(false));
        let sound = listener.map(|l| {
            let (feed, done) = (feed.clone(), done.clone());
            std::thread::spawn(move || {
                read_sound(&l, &done, |pcm| {
                    let span = CHUNK as u64 * 1_000_000_000 / 48_000;
                    let f = feed.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    f.sound(pcm, 48_000, now_ns().saturating_sub(span));
                });
            })
        });
        if let Some(mut out) = out {
            let (w, h) = SIZE;
            let mut frame = vec![0u8; (w * h * 4) as usize];
            while !stop.load(Ordering::Acquire) && out.read_exact(&mut frame).is_ok() {
                let f = feed.lock().unwrap_or_else(|e| e.into_inner()).clone();
                f.picture(ferrocore::Picture { width: w, height: h, rgba: frame.clone(), premultiplied: false });
            }
        }
        if let Some(mut c) = child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        // FFmpeg has ended: a sound reader still waiting for it to connect stops waiting.
        done.store(true, Ordering::Release);
        if let Some(sound) = sound {
            let _ = sound.join();
        }
        if let Some(text) = errors.and_then(|t| t.join().ok()) {
            let next = parts_after(parts, &text);
            if next != parts {
                tracing::info!(?next, "a network media source: asking FFmpeg only for what the stream has");
                parts = next;
                continue;
            }
        }
        // In short steps, so stopping never waits out a long reconnect delay.
        let until = std::time::Instant::now() + delay;
        while !stop.load(Ordering::Acquire) && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// The sound FFmpeg sends (48 kHz stereo f32), a chunk at a time to `chunk`, once it connects
/// to `l`. Gives up waiting when `done` is set (FFmpeg ended without connecting); only the first
/// connection is read.
fn read_sound(l: &TcpListener, done: &AtomicBool, mut chunk: impl FnMut(Vec<f32>)) {
    if l.set_nonblocking(true).is_err() {
        return;
    }
    let mut conn = loop {
        match l.accept() {
            Ok((c, _)) => break c,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if done.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return,
        }
    };
    if conn.set_nonblocking(false).is_err() {
        return;
    }
    let mut buf = vec![0u8; CHUNK * 2 * 4];
    while conn.read_exact(&mut buf).is_ok() {
        chunk(buf.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn obs_network_settings_become_ffmpeg_arguments() {
        let set = json!({ "input": "/tmp/guest.sdp", "input_format": "sdp", "ffmpeg_options": "protocol_whitelist=file,udp,rtp fflags=nobuffer bad;key=x" });
        let a = args(&set, "tcp://127.0.0.1:9", BOTH).unwrap_or_default();
        let s = a.join(" ");
        assert!(
            s.contains(
                "-f sdp -analyzeduration 1000000 -protocol_whitelist file,udp,rtp -fflags nobuffer -hwaccel auto -i /tmp/guest.sdp"
            ),
            "{s}"
        );
        assert!(!s.contains("bad;key"), "only plain option names pass");
        let own =
            args(&json!({ "input": "x", "ffmpeg_options": "analyzeduration=5" }), "tcp://127.0.0.1:9", BOTH)
                .unwrap_or_default();
        assert_eq!(own.iter().filter(|a| *a == "-analyzeduration").count(), 1, "the source's own probe time wins");
        assert!(s.ends_with("tcp://127.0.0.1:9"));
        assert!(args(&json!({}), "tcp://127.0.0.1:9", BOTH).is_none(), "no address, nothing to read");
        let picture = Parts { picture: true, sound: false };
        let only = args(&set, "tcp://127.0.0.1:9", picture).unwrap_or_default().join(" ");
        assert!(only.ends_with("pipe:1") && !only.contains("0:a:0"), "{only}");
        // Sound alone isn't decoded on the GPU.
        let sound = Parts { picture: false, sound: true };
        let only = args(&set, "tcp://127.0.0.1:9", sound).unwrap_or_default().join(" ");
        assert!(!only.contains("hwaccel"), "{only}");
    }

    #[test]
    fn a_network_stream_reaches_the_engine() {
        reaches_the_engine(true);
    }

    #[test]
    fn a_stream_with_no_sound_still_shows() {
        reaches_the_engine(false);
    }

    fn reaches_the_engine(with_sound: bool) {
        use ferrocore::engine::{Engine, EngineConfig, SceneItem, SourceKind};
        let Ok(ff) = which("ffmpeg") else { return };
        let port =
            std::net::UdpSocket::bind("127.0.0.1:0").and_then(|s| s.local_addr()).map(|a| a.port()).unwrap_or(23999);
        let mut send = Command::new(&ff);
        send.args(["-v", "error", "-re", "-f", "lavfi", "-i", "color=c=0x00ff00:size=640x360:rate=30"]);
        if with_sound {
            send.args(["-f", "lavfi", "-i", "sine=frequency=440", "-c:a", "mp2"]);
        }
        let mut sender = send
            .args(["-c:v", "mpeg2video", "-f", "mpegts", &format!("udp://127.0.0.1:{port}?pkt_size=1316")])
            .stdin(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("{e}"));
        ferrocore::ffenc::set_ffmpeg(&ff);
        let video = ferrocore::video::VideoConfig {
            codec: ferrocore::video::VideoCodec::H264,
            input: ferrocore::video::PixelFormat::Nv12,
            width: 1280,
            height: 720,
            fps_num: 30,
            fps_den: 1,
            bitrate_kbps: 2000,
            rate: ferrocore::video::RateControl::Cbr,
            keyframe_secs: 2,
            software: false,
        };
        let audio = ferrocore::audio::AudioConfig { sample_rate: 48_000, channels: 2, bitrate_kbps: 128 };
        let Ok(e) = Engine::new(EngineConfig { width: 1280, height: 720, fps: 30, ..Default::default() }, video, audio)
        else {
            let _ = sender.kill();
            let _ = sender.wait();
            return;
        };
        e.set_source(1, SourceKind::Fed { width: 1280, height: 720 }).unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Main", vec![SceneItem::new(1, ferrocore::Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        let feed = e.fed(1).unwrap_or_else(|| panic!("fed"));
        let m = NetMedia::start(&json!({ "input": format!("udp://127.0.0.1:{port}") }), feed)
            .unwrap_or_else(|x| panic!("{x}"));
        let mid = (360 * 1280 + 640) * 4;
        let mut px = Vec::new();
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(250));
            px = e.snapshot().unwrap_or_default();
            if green(&px, mid) {
                break;
            }
        }
        drop(m);
        let _ = sender.kill();
        let _ = sender.wait();
        assert!(green(&px, mid), "the stream's green, in the middle: {:?}", px.get(mid..mid + 3));
    }

    #[test]
    fn sound_comes_through_a_local_connection() {
        let Ok(ff) = which("ffmpeg") else { return };
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap_or_else(|e| panic!("{e}"));
        let to = format!("tcp://{}", l.local_addr().unwrap_or_else(|e| panic!("{e}")));
        let mut sender = Command::new(&ff)
            .args(["-v", "error", "-f", "lavfi", "-i", "sine=frequency=440:duration=1", "-f", "f32le", "-ar", "48000"])
            .args(["-ac", "2", &to])
            .stdin(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("{e}"));
        let done = AtomicBool::new(false);
        let mut got = 0usize;
        let mut loud = 0f32;
        read_sound(&l, &done, |pcm| {
            got += pcm.len();
            loud = pcm.iter().fold(loud, |m, v| m.max(v.abs()));
        });
        let _ = sender.wait();
        // A second's tone, less the part that doesn't fill a whole chunk.
        assert!(got >= 46 * CHUNK * 2 && loud > 0.05, "{got} samples, peak {loud}");
        // FFmpeg gone without connecting: the reader doesn't wait forever.
        done.store(true, Ordering::Release);
        read_sound(&l, &done, |_| panic!("nothing connected"));
    }

    #[test]
    fn a_stream_without_sound_or_picture_is_asked_for_what_it_has() {
        let none = "[out#1/f32le @ 0x1] Output file does not contain any stream\nError opening output file";
        assert_eq!(parts_after(BOTH, none), Parts { picture: true, sound: false });
        let no_picture = "[out#0/rawvideo @ 0x1] Output file does not contain any stream";
        assert_eq!(parts_after(BOTH, no_picture), Parts { picture: false, sound: true });
        assert_eq!(parts_after(BOTH, "Connection refused"), BOTH, "other errors change nothing");
        let sound_only = Parts { picture: false, sound: true };
        assert_eq!(parts_after(sound_only, no_picture), BOTH, "nothing at all yet: ask for both again");
    }

    fn green(px: &[u8], at: usize) -> bool {
        px.get(at..at + 3).is_some_and(|p| p[0] < 3 && p[1] > 252 && p[2] < 3)
    }

    fn which(name: &str) -> Result<std::path::PathBuf, ()> {
        std::env::var_os("PATH")
            .and_then(|p| std::env::split_paths(&p).map(|d| d.join(name)).find(|f| f.is_file()))
            .ok_or(())
    }
}
