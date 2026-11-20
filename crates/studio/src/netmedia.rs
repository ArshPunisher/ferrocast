//! Media from a network address (a guest's camera over WebRTC, described by an SDP file; an
//! RTMP, HLS or SRT address) for Ferrocast's own engine: FFmpeg reads it, as OBS's Media source
//! did, and hands its pictures (fitted into `SIZE`) and sound (48 kHz stereo) to a fed source.
//! When the stream stalls or drops, FFmpeg is killed and started again with bounded backoff.

use std::collections::VecDeque;
use std::io::Read;
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ferrocore::engine::{Feed, now_ns};
use serde_json::Value;

/// The size pictures are fitted into (their shape kept, the rest clear).
pub(crate) const SIZE: (u32, u32) = (1280, 720);
/// Sound read at a time (stereo frames).
const CHUNK: usize = 1024;
/// Default network I/O and stall timeout before killing FFmpeg and reconnecting.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an attempt must deliver before it counts as healthy and the backoff starts over. A
/// server that sends a frame and drops is not healthy: it is retried after the growing wait.
const HEALTHY_AFTER: Duration = Duration::from_secs(10);
/// Maximum stderr bytes kept in memory for diagnostics (the pipe is always drained to EOF).
const STDERR_KEEP: usize = 8 * 1024;

/// Safe FFmpeg demuxer/protocol input options accepted from `ffmpeg_options` (matching OBS's
/// `av_dict_parse_string` scope and rejecting output/filter/file CLI flag injection).
const ALLOWED_FFMPEG_OPTS: &[&str] = &[
    "analyzeduration",
    "auth_type",
    "avioflags",
    "block",
    "buffer_size",
    "ca_file",
    "connect",
    "cookies",
    "crypto_suite",
    "fflags",
    "fifo_size",
    "flags",
    "headers",
    "http_persistent",
    "http_proxy",
    "hwaccel",
    "icy",
    "latency",
    "listen_timeout",
    "live_start_index",
    "localaddr",
    "localport",
    "max_delay",
    "mode",
    "multiple_requests",
    "overrun_nonfatal",
    "passphrase",
    "pbkeylen",
    "pkt_size",
    "probesize",
    "protocol_whitelist",
    "reconnect",
    "reconnect_at_eof",
    "reconnect_delay_max",
    "reconnect_on_http_error",
    "reconnect_on_network_error",
    "reconnect_streamed",
    "referer",
    "reorder_queue_size",
    "reuse",
    "rtmp_app",
    "rtmp_buffer",
    "rtmp_conn",
    "rtmp_flashver",
    "rtmp_live",
    "rtmp_pageurl",
    "rtmp_playpath",
    "rtmp_swfurl",
    "rtmp_tcurl",
    "rtsp_flags",
    "rtsp_transport",
    "rw_timeout",
    "seekable",
    "sources",
    "srt_streamid",
    "srtp_in_params",
    "srtp_in_suite",
    "stimeout",
    "streamid",
    "timeout",
    "tls_verify",
    "transtype",
    "ttl",
    "user_agent",
];

/// One network source being read. Dropping it stops FFmpeg.
pub(crate) struct NetMedia {
    feed: Arc<Mutex<Feed>>,
    stop: Arc<AtomicBool>,
    wake: Arc<(Mutex<()>, Condvar)>,
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

/// Parses OBS's `ffmpeg_options` string into `(key, value)` pairs, respecting single/double
/// quotes and backslash escapes (matching `av_dict_parse_string`), and keeps only keys in
/// [`ALLOWED_FFMPEG_OPTS`].
fn parse_ffmpeg_options(opts: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut chars = opts.chars().peekable();
    while chars.peek().is_some() {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut token = String::new();
        let mut quote: Option<char> = None;
        while let Some(c) = chars.next() {
            match quote {
                Some(q) if c == q => quote = None,
                Some('"') if c == '\\' => {
                    if let Some(next) = chars.next() {
                        token.push(next);
                    }
                }
                Some(_) => token.push(c),
                None if c == '"' || c == '\'' => quote = Some(c),
                None if c == '\\' => {
                    if let Some(next) = chars.next() {
                        token.push(next);
                    }
                }
                None if c.is_whitespace() => break,
                None => token.push(c),
            }
        }
        if let Some((k, v)) = token.split_once('=')
            && ALLOWED_FFMPEG_OPTS.contains(&k)
        {
            out.push((k.to_string(), v.to_string()));
        }
    }
    out
}

/// Returns the FFmpeg protocol timeout option (`-rw_timeout` or `-timeout`) appropriate for
/// `input` and `input_format`, or `None` for formats (like `sdp`) that reject AVIO timeouts.
fn protocol_timeout_flag(input: &str, input_format: Option<&str>) -> Option<&'static str> {
    if input_format.is_some_and(|f| f.eq_ignore_ascii_case("sdp")) {
        return None;
    }
    let lower = input.trim().to_ascii_lowercase();
    if lower.starts_with("rtsp://") || lower.starts_with("rtsps://") {
        return Some("timeout");
    }
    const RW_SCHEMES: &[&str] = &[
        "http://", "https://", "rtmp://", "rtmps://", "rtmpt://", "rtmpe://", "tcp://", "tls://", "srt://", "udp://",
        "rtp://", "mmsh://", "mmst://",
    ];
    RW_SCHEMES.iter().any(|s| lower.starts_with(s)).then_some("rw_timeout")
}

/// The FFmpeg arguments for a Media source's network settings (OBS's keys: `input`,
/// `input_format`, `ffmpeg_options`, `hw_decode`), its picture to stdout and its sound to
/// `sound_to` (an FFmpeg output address).
fn args(set: &Value, sound_to: &str, parts: Parts, use_hw: bool, timeout: Duration) -> Option<Vec<String>> {
    let input = set.get("input").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin"].map(String::from).to_vec();
    let input_format = set
        .get("input_format")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ',')));
    if let Some(f) = input_format {
        a.extend(["-f".into(), f.into()]);
    }
    // OBS's "FFmpeg options": quoted `key=value` pairs filtered through the safe input allowlist.
    let opts = set.get("ffmpeg_options").and_then(Value::as_str).unwrap_or_default();
    let parsed = parse_ffmpeg_options(opts);
    let has_opt = |name: &str| parsed.iter().any(|(k, _)| k == name);
    // A live stream shows within a second, not after FFmpeg's default five of probing.
    if !has_opt("analyzeduration") {
        a.extend(["-analyzeduration".into(), "1000000".into()]);
    }
    // Bound socket reads so half-open TCP/RTMP/RTSP/HLS connections abort instead of hanging forever.
    if let Some(flag) = protocol_timeout_flag(input, input_format) {
        let already = has_opt(flag) || (flag == "timeout" && has_opt("stimeout"));
        if !already {
            let us = timeout.as_micros().max(1_000_000);
            a.extend([format!("-{flag}"), us.to_string()]);
        }
    }
    for (k, v) in &parsed {
        a.extend([format!("-{k}"), v.clone()]);
    }
    // Pictures decoded on the GPU when allowed and not overridden, with CPU fallback in `run()`.
    let hw_setting = set.get("hw_decode").and_then(Value::as_bool).unwrap_or(true);
    if parts.picture && use_hw && hw_setting && ferrocore::ffsrc::hw_decode_allowed() && !has_opt("hwaccel") {
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

/// Drains FFmpeg's stderr pipe completely to EOF into a bounded ring buffer so FFmpeg never
/// receives `SIGPIPE` after writing more than `STDERR_KEEP` bytes of warnings/logs.
fn drain_stderr(mut err: impl Read) -> String {
    let mut ring: VecDeque<u8> = VecDeque::new();
    let mut buf = [0u8; 4096];
    loop {
        match err.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                for &b in &buf[..n] {
                    if ring.len() == STDERR_KEEP {
                        ring.pop_front();
                    }
                    ring.push_back(b);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    let bytes: Vec<u8> = ring.into_iter().collect();
    String::from_utf8_lossy(&bytes).into_owned()
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
        Self::start_with_timeout(set, feed, DEFAULT_TIMEOUT)
    }

    fn start_with_timeout(set: &Value, feed: Feed, timeout: Duration) -> Result<Self, String> {
        let ffmpeg = ferrocore::ffenc::path()
            .ok_or("Media from a network address needs FFmpeg: pick x264 once in Settings and Ferrocast fetches it.")?;
        args(set, "", BOTH, true, timeout).ok_or("The media source has no address to read.")?;
        let set = set.clone();
        let delay = set.get("reconnect_delay_sec").and_then(Value::as_u64).unwrap_or(1).clamp(1, 60);
        let feed = Arc::new(Mutex::new(feed));
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new((Mutex::new(()), Condvar::new()));
        let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let thread = {
            let (feed, stop, wake, child) = (feed.clone(), stop.clone(), wake.clone(), child.clone());
            std::thread::Builder::new()
                .name("ferrocast-net-media".into())
                .spawn(move || run(&ffmpeg, &set, &feed, &stop, &wake, &child, Duration::from_secs(delay), timeout))
                .map_err(|e| format!("The media source couldn't start: {e}"))?
        };
        Ok(Self { feed, stop, wake, child, thread: Some(thread) })
    }

    /// Our engine's source changed (the collection was opened again): its new feed.
    pub(crate) fn set_feed(&self, feed: Feed) {
        *self.feed.lock().unwrap_or_else(|e| e.into_inner()) = feed;
    }
}

impl Drop for NetMedia {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        {
            let _g = self.wake.0.lock().unwrap_or_else(|e| e.into_inner());
            self.wake.1.notify_all();
        }
        if let Some(mut c) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Sleeps up to `wait`, waking immediately if `stop` is signalled.
fn sleep_or_stop(stop: &AtomicBool, wake: &(Mutex<()>, Condvar), wait: Duration) {
    let deadline = Instant::now() + wait;
    let mut g = wake.0.lock().unwrap_or_else(|e| e.into_inner());
    while !stop.load(Ordering::Acquire) {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else { break };
        if left.is_zero() {
            break;
        }
        g = wake.1.wait_timeout(g, left).unwrap_or_else(|e| e.into_inner()).0;
    }
}

/// FFmpeg, started again with bounded exponential backoff whenever it stalls or ends, until stopped.
#[allow(clippy::too_many_arguments)]
fn run(
    ffmpeg: &std::path::Path,
    set: &Value,
    feed: &Arc<Mutex<Feed>>,
    stop: &Arc<AtomicBool>,
    wake: &Arc<(Mutex<()>, Condvar)>,
    child: &Arc<Mutex<Option<Child>>>,
    base_delay: Duration,
    stall_timeout: Duration,
) {
    let mut parts = BOTH;
    let mut use_hw = set.get("hw_decode").and_then(Value::as_bool).unwrap_or(true);
    let mut failures: u32 = 0;
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
        let Some(args) = args(set, &sound_to, parts, use_hw, stall_timeout) else { return };
        let spawned = ferrocore::ffenc::hidden(Command::new(ffmpeg))
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let Ok(mut c) = spawned else {
            tracing::warn!("FFmpeg couldn't start for a network media source");
            return;
        };
        let out = c.stdout.take();
        // Drain stderr completely to EOF into a bounded ring buffer so FFmpeg never hits SIGPIPE.
        let errors = c.stderr.take().map(|e| std::thread::spawn(move || drain_stderr(e)));
        {
            // Stopped while FFmpeg was starting: `Drop` has looked for it already, so end it here.
            let mut slot = child.lock().unwrap_or_else(|e| e.into_inner());
            if stop.load(Ordering::Acquire) {
                let _ = c.kill();
                let _ = c.wait();
                if let Some(t) = errors {
                    let _ = t.join();
                }
                return;
            }
            *slot = Some(c);
        }
        let done = Arc::new(AtomicBool::new(false));
        let last_activity = Arc::new(AtomicU64::new(now_ns()));
        let delivered = Arc::new(AtomicBool::new(false));
        // When the first picture or sound chunk came (ns; 0: nothing yet) and when the last did.
        let first_at = Arc::new(AtomicU64::new(0));
        let timed_out = Arc::new(AtomicBool::new(false));
        let attempt_wake = Arc::new((Mutex::new(()), Condvar::new()));

        // Watchdog kills FFmpeg if neither picture nor sound arrives within `stall_timeout`
        // (catches half-open TCP sockets, silent UDP/SDP, and wedged decoders).
        let watchdog = {
            let (done, stop_flag, last_activity, timed_out, child, attempt_wake) = (
                done.clone(),
                stop.clone(),
                last_activity.clone(),
                timed_out.clone(),
                child.clone(),
                attempt_wake.clone(),
            );
            let timeout_ns = stall_timeout.as_nanos().min(u128::from(u64::MAX)) as u64;
            std::thread::spawn(move || {
                let mut g = attempt_wake.0.lock().unwrap_or_else(|e| e.into_inner());
                while !done.load(Ordering::Acquire) && !stop_flag.load(Ordering::Acquire) {
                    let elapsed = now_ns().saturating_sub(last_activity.load(Ordering::Relaxed));
                    if elapsed >= timeout_ns {
                        timed_out.store(true, Ordering::Release);
                        tracing::warn!(
                            timeout_ms = timeout_ns / 1_000_000,
                            "a network media source timed out waiting for data: reconnecting"
                        );
                        if let Some(c) = child.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                            let _ = c.kill();
                        }
                        return;
                    }
                    let wait = Duration::from_nanos((timeout_ns - elapsed).min(200_000_000));
                    g = attempt_wake.1.wait_timeout(g, wait).unwrap_or_else(|e| e.into_inner()).0;
                }
            })
        };

        // Sound on its own thread (it waits for FFmpeg to connect).
        let sound = listener.map(|l| {
            let (feed, done, last_activity, delivered, first_at) =
                (feed.clone(), done.clone(), last_activity.clone(), delivered.clone(), first_at.clone());
            std::thread::spawn(move || {
                read_sound(&l, &done, |pcm| {
                    last_activity.store(now_ns(), Ordering::Relaxed);
                    let _ = first_at.compare_exchange(0, now_ns(), Ordering::Relaxed, Ordering::Relaxed);
                    delivered.store(true, Ordering::Release);
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
                last_activity.store(now_ns(), Ordering::Relaxed);
                let _ = first_at.compare_exchange(0, now_ns(), Ordering::Relaxed, Ordering::Relaxed);
                delivered.store(true, Ordering::Release);
                let f = feed.lock().unwrap_or_else(|e| e.into_inner()).clone();
                f.picture(ferrocore::Picture { width: w, height: h, rgba: frame.clone(), premultiplied: false });
            }
        }

        // FFmpeg has ended (or stalled): wake watchdog and sound reader, then reap child.
        done.store(true, Ordering::Release);
        {
            let _g = attempt_wake.0.lock().unwrap_or_else(|e| e.into_inner());
            attempt_wake.1.notify_all();
        }
        if let Some(mut c) = child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = watchdog.join();
        if let Some(sound) = sound {
            let _ = sound.join();
        }
        let err_text = errors.and_then(|t| t.join().ok()).unwrap_or_default();
        if stop.load(Ordering::Acquire) {
            return;
        }
        let next = parts_after(parts, &err_text);
        if next != parts {
            tracing::info!(?next, "a network media source: asking FFmpeg only for what the stream has");
            parts = next;
            continue;
        }
        // Hardware decoding stops only when FFmpeg blamed the decoder for an attempt that
        // delivered nothing (not a refused connection, a 404 or a quiet stream).
        let got_any = delivered.load(Ordering::Acquire);
        let hw_next = hardware_after(use_hw, got_any, timed_out.load(Ordering::Acquire), &err_text);
        if use_hw && !hw_next && parts.picture {
            tracing::info!("a network media source: the decoder failed, hardware decoding is off for it");
        }
        use_hw = hw_next;
        // How long it delivered this time: only a good stretch starts the wait over.
        let first = first_at.load(Ordering::Relaxed);
        let good_for = (got_any && first > 0)
            .then(|| Duration::from_nanos(last_activity.load(Ordering::Relaxed).saturating_sub(first)));
        failures = failures_after(failures, good_for);
        sleep_or_stop(stop, wake, backoff_after(base_delay, failures));
    }
}

/// Whether FFmpeg's error text blames the decoder or the hardware behind it (and not the
/// network): only then is hardware decoding turned off for good. A server that is down, a 404 or
/// a stream that went quiet say nothing about the decoder.
fn decoder_trouble(errors: &str) -> bool {
    let e = errors.to_ascii_lowercase();
    const SIGNS: &[&str] = &[
        "hwaccel",
        "hardware",
        "videotoolbox",
        "vaapi",
        "vdpau",
        "cuda",
        "nvdec",
        "cuvid",
        "dxva",
        "d3d11",
        "qsv",
        "failed setup for format",
        "no device available for decoder",
        "error while decoding",
        "could not open codec",
        "failed to open codec",
        "decoder",
    ];
    SIGNS.iter().any(|s| e.contains(s))
}

/// Whether the next attempt decodes on the hardware: it stops only after an attempt that
/// delivered nothing and whose error text blames the decoder (never a refused connection, a 404
/// or our own watchdog).
fn hardware_after(was_on: bool, delivered: bool, timed_out: bool, errors: &str) -> bool {
    was_on && (delivered || timed_out || !decoder_trouble(errors))
}

/// How many attempts in a row have been no good, after one that delivered for `healthy_for`
/// (None: nothing arrived): only a stretch of good frames starts the backoff over.
fn failures_after(failures: u32, delivered_for: Option<Duration>) -> u32 {
    match delivered_for {
        Some(d) if d >= HEALTHY_AFTER => 0,
        _ => failures.saturating_add(1),
    }
}

/// The wait before the next attempt: the source's own delay, doubling for each attempt in a row
/// that wasn't good, up to a minute.
fn backoff_after(base: Duration, failures: u32) -> Duration {
    Duration::from_secs((base.as_secs().max(1) << failures.saturating_sub(1).min(5)).min(60))
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
    while !done.load(Ordering::Acquire) && conn.read_exact(&mut buf).is_ok() {
        chunk(buf.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_ffmpeg_options_handles_quotes_and_rejects_unsafe_flags() {
        let parsed = parse_ffmpeg_options(
            r#"headers="Authorization: Bearer tok 123" user_agent='Custom Agent/1.0' filter_complex=evil y=/tmp/pwn i=/etc/passwd fflags=nobuffer bad;key=x"#,
        );
        assert_eq!(
            parsed,
            vec![
                ("headers".into(), "Authorization: Bearer tok 123".into()),
                ("user_agent".into(), "Custom Agent/1.0".into()),
                ("fflags".into(), "nobuffer".into()),
            ]
        );
    }

    #[test]
    fn obs_network_settings_become_ffmpeg_arguments() {
        let set = json!({
            "input": "/tmp/guest.sdp",
            "input_format": "sdp",
            "ffmpeg_options": "protocol_whitelist=file,udp,rtp fflags=nobuffer bad;key=x filter_complex=x"
        });
        let a = args(&set, "tcp://127.0.0.1:9", BOTH, true, DEFAULT_TIMEOUT).unwrap_or_default();
        let s = a.join(" ");
        assert!(
            s.contains(
                "-f sdp -analyzeduration 1000000 -protocol_whitelist file,udp,rtp -fflags nobuffer -hwaccel auto -i /tmp/guest.sdp"
            ),
            "{s}"
        );
        assert!(!s.contains("bad;key") && !s.contains("filter_complex"), "only allowlisted options pass: {s}");
        assert!(!s.contains("rw_timeout"), "sdp must not receive -rw_timeout: {s}");

        // Network URLs get protocol-appropriate timeout flags unless overridden.
        let rtmp =
            args(&json!({ "input": "rtmp://live.test/app/key" }), "tcp://127.0.0.1:9", BOTH, true, DEFAULT_TIMEOUT)
                .unwrap_or_default()
                .join(" ");
        assert!(rtmp.contains("-rw_timeout 10000000"), "{rtmp}");
        let rtsp = args(&json!({ "input": "rtsp://cam.test/live" }), "tcp://127.0.0.1:9", BOTH, true, DEFAULT_TIMEOUT)
            .unwrap_or_default()
            .join(" ");
        assert!(rtsp.contains("-timeout 10000000"), "{rtsp}");
        let no_hw =
            args(&json!({ "input": "http://cam.test/a.m3u8", "hw_decode": false }), "", BOTH, true, DEFAULT_TIMEOUT)
                .unwrap_or_default()
                .join(" ");
        assert!(!no_hw.contains("hwaccel"), "hw_decode=false disables hwaccel: {no_hw}");

        let own = args(
            &json!({ "input": "x", "ffmpeg_options": "analyzeduration=5" }),
            "tcp://127.0.0.1:9",
            BOTH,
            true,
            DEFAULT_TIMEOUT,
        )
        .unwrap_or_default();
        assert_eq!(own.iter().filter(|a| *a == "-analyzeduration").count(), 1, "the source's own probe time wins");
        assert!(s.ends_with("tcp://127.0.0.1:9"));
        assert!(
            args(&json!({}), "tcp://127.0.0.1:9", BOTH, true, DEFAULT_TIMEOUT).is_none(),
            "no address, nothing to read"
        );
        let picture = Parts { picture: true, sound: false };
        let only = args(&set, "tcp://127.0.0.1:9", picture, true, DEFAULT_TIMEOUT).unwrap_or_default().join(" ");
        assert!(only.ends_with("pipe:1") && !only.contains("0:a:0"), "{only}");
        // Sound alone isn't decoded on the GPU.
        let sound = Parts { picture: false, sound: true };
        let only = args(&set, "tcp://127.0.0.1:9", sound, true, DEFAULT_TIMEOUT).unwrap_or_default().join(" ");
        assert!(!only.contains("hwaccel"), "{only}");
    }

    /// A dead server or a 404 says nothing about the decoder: hardware decoding stays on and the
    /// wait keeps growing; only the decoder's own errors turn it off (M8-05).
    #[test]
    fn only_a_decoder_error_turns_hardware_decoding_off() {
        for net in [
            "tcp://127.0.0.1:1: Connection refused",
            "Server returned 404 Not Found\nhttp://cam.test/a.m3u8: Server returned 404 Not Found",
            "rtmp://live.test/app/key: Input/output error",
            "Connection timed out",
            "",
        ] {
            assert!(!decoder_trouble(net), "{net:?}");
            assert!(hardware_after(true, false, false, net), "nothing delivered, but not the decoder: {net:?}");
        }
        for dec in [
            "[h264 @ 0x1] Failed setup for format videotoolbox_vld: hwaccel initialisation returned error.",
            "No device available for decoder: device type cuda needed for codec h264_cuvid.",
            "Hardware device setup failed for decoder on device type d3d11va",
            "Could not open codec h264",
        ] {
            assert!(decoder_trouble(dec), "{dec:?}");
            assert!(!hardware_after(true, false, false, dec), "off after a decoder error: {dec:?}");
            assert!(hardware_after(true, true, false, dec), "it delivered, so it worked");
            assert!(hardware_after(true, false, true, dec), "our own watchdog killed it");
        }
        // Off stays off.
        assert!(!hardware_after(false, true, false, ""));
    }

    /// The wait starts over only after 10 s of good frames (a server that sends one frame and
    /// drops is retried after the growing wait, not every second), doubling to a minute.
    #[test]
    fn the_wait_starts_over_only_after_a_good_stretch() {
        let s = Duration::from_secs;
        let mut f = 0;
        let mut waits = Vec::new();
        for _ in 0..8 {
            f = failures_after(f, None);
            waits.push(backoff_after(s(1), f).as_secs());
        }
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 32, 32], "doubling, and held (the cap is 5 doublings)");
        // One frame, then a drop: still failing, the wait goes on growing.
        assert_eq!(failures_after(3, Some(Duration::from_millis(40))), 4);
        assert_eq!(failures_after(3, Some(s(9))), 4);
        // Ten seconds of delivery is healthy: starts over.
        assert_eq!(failures_after(5, Some(s(10))), 0);
        assert_eq!(backoff_after(s(1), 0), s(1));
        // A source's own delay is the base, and a minute is the most.
        assert_eq!(backoff_after(s(10), 3), s(40));
        assert_eq!(backoff_after(s(10), 6), s(60));
    }

    /// Half-open TCP stream that stalls after accepting must time out and reconnect rather than
    /// blocking `read_exact` forever (M8-05).
    #[test]
    fn stalled_tcp_stream_times_out_and_reconnects() {
        let Ok(ff) = which("ffmpeg") else { return };
        ferrocore::ffenc::set_ffmpeg(&ff);
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap_or_else(|e| panic!("{e}"));
        let port = listener.local_addr().unwrap_or_else(|e| panic!("{e}")).port();
        listener.set_nonblocking(true).unwrap_or_else(|e| panic!("{e}"));
        let hits = Arc::new(AtomicU64::new(0));
        let srv_stop = Arc::new(AtomicBool::new(false));
        let server = {
            let (hits, srv_stop) = (hits.clone(), srv_stop.clone());
            std::thread::spawn(move || {
                let mut held = Vec::new();
                while !srv_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((conn, _)) => {
                            hits.fetch_add(1, Ordering::Relaxed);
                            // Keep socket open without sending data (half-open blackhole).
                            held.push(conn);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        let feed = Feed::default();
        let m = NetMedia::start_with_timeout(
            &json!({
                "input": format!("tcp://127.0.0.1:{port}"),
                "input_format": "mpegts",
                "reconnect_delay_sec": 1,
                "hw_decode": false,
            }),
            feed,
            Duration::from_secs(1),
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let until = Instant::now() + Duration::from_secs(7);
        while Instant::now() < until && hits.load(Ordering::Relaxed) < 2 {
            std::thread::sleep(Duration::from_millis(50));
        }
        let count = hits.load(Ordering::Relaxed);
        drop(m);
        srv_stop.store(true, Ordering::Release);
        let _ = server.join();
        assert!(count >= 2, "stalled TCP blackhole must time out and trigger reconnect (saw {count} connections)");
    }

    /// Emitting far more than 8 KB on stderr must never close the pipe early or kill FFmpeg with
    /// SIGPIPE (M8-05).
    #[test]
    fn drain_stderr_reads_past_8kb_without_closing_pipe() {
        let Ok(ff) = which("ffmpeg") else { return };
        // `-loglevel debug` on a 1 s lavfi input emits >50 KB to stderr; closing stderr at 8 KB
        // would kill FFmpeg via SIGPIPE before it completes.
        let mut child = Command::new(&ff)
            .args([
                "-loglevel",
                "debug",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=32x32:r=30:d=0.5",
                "-f",
                "null",
                "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("{e}"));
        let err = child.stderr.take().unwrap_or_else(|| panic!("stderr"));
        let t = std::thread::spawn(move || drain_stderr(err));
        let status = child.wait().unwrap_or_else(|e| panic!("{e}"));
        let kept = t.join().unwrap_or_else(|_| panic!("stderr thread"));
        assert!(status.success(), "FFmpeg must exit cleanly without SIGPIPE: {status}");
        assert!(kept.len() <= STDERR_KEEP, "kept ring must stay bounded: {}", kept.len());
        assert!(!kept.is_empty(), "retained tail of stderr");
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
            profile: ferrocore::video::H264Profile::High,
            preset: None,
            tune: None,
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
