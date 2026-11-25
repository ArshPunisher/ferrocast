//! Pure-logic helpers for Windows capture (format negotiation, pacing, monitor matching),
//! compiled unconditionally in tests so `cargo test` on Mac exercises them directly.

use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::engine::CameraSpec;

/// How long DirectShow device enumeration results are reused so 1 s polling does not respawn FFmpeg.
pub const DSHOW_CAMERA_CACHE_TTL: Duration = Duration::from_secs(2);

struct CachedDshowProbe<T> {
    in_flight: bool,
    cached_at: Option<Instant>,
    last: Vec<T>,
}

/// Coalesces concurrent DirectShow enumerations and serves recent device snapshots for
/// [`DSHOW_CAMERA_CACHE_TTL`], recovering cleanly if a worker panics while probing.
pub struct DshowCameraCache<T> {
    state: Mutex<CachedDshowProbe<T>>,
    cv: Condvar,
    ttl: Duration,
}

impl<T> Default for DshowCameraCache<T> {
    fn default() -> Self {
        Self::new(DSHOW_CAMERA_CACHE_TTL)
    }
}

impl<T> DshowCameraCache<T> {
    pub fn new(ttl: Duration) -> Self {
        Self {
            state: Mutex::new(CachedDshowProbe { in_flight: false, cached_at: None, last: Vec::new() }),
            cv: Condvar::new(),
            ttl,
        }
    }
}

impl<T: Clone> DshowCameraCache<T> {
    pub fn get_or_probe(&self, now: impl Fn() -> Instant, probe: impl FnOnce() -> Option<Vec<T>>) -> Vec<T> {
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.in_flight {
            while guard.in_flight {
                guard = self.cv.wait(guard).unwrap_or_else(PoisonError::into_inner);
            }
            return guard.last.clone();
        }
        let check_at = now();
        if let Some(at) = guard.cached_at
            && check_at.saturating_duration_since(at) < self.ttl
        {
            return guard.last.clone();
        }
        guard.in_flight = true;
        drop(guard);

        struct ResetFlight<'a, T>(&'a Mutex<CachedDshowProbe<T>>, &'a Condvar);
        impl<T> Drop for ResetFlight<'_, T> {
            fn drop(&mut self) {
                let mut g = self.0.lock().unwrap_or_else(PoisonError::into_inner);
                g.in_flight = false;
                self.1.notify_all();
            }
        }
        let _reset = ResetFlight(&self.state, &self.cv);

        let probed = probe();
        let done_at = now();
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let out = match probed {
            Some(list) => {
                guard.last = list.clone();
                guard.cached_at = Some(done_at);
                list
            }
            None => {
                guard.cached_at = None;
                guard.last.clear();
                Vec::new()
            }
        };
        guard.in_flight = false;
        self.cv.notify_all();
        out
    }
}

/// Frame-arrival pacing for Windows.Graphics.Capture.
///
/// Compares elapsed time against `0.9 * interval` (like OBS's ScreenCaptureKit pacer) so
/// 60 Hz display frames arriving with ~16 ms jitter aren't dropped after `TryGetNextFrame`,
/// while high-refresh displays (144/240 Hz) or lower fps caps still skip redundant readbacks.
/// When a frame is rejected near the end of a burst, `defer` / `flush_due` lets the capture
/// materialize that final texture once the cadence window opens so a still screen never freezes
/// one frame behind.
#[derive(Debug, Clone)]
pub struct WgcPacer {
    every: Duration,
    anchor: Option<Instant>,
    last_taken: Option<Instant>,
    deferred_at: Option<Instant>,
}

impl WgcPacer {
    pub fn new(fps: u32) -> Self {
        Self {
            every: Duration::from_secs_f64(1.0 / f64::from(fps.clamp(1, 240))),
            anchor: None,
            last_taken: None,
            deferred_at: None,
        }
    }

    fn ready(&self, now: Instant) -> bool {
        let Some(anchor) = self.anchor else {
            return true;
        };
        if now.saturating_duration_since(anchor) < self.every.mul_f64(0.9) {
            return false;
        }
        // Never accept two frames closer than half an interval apart (e.g. a burst after an OS drop).
        !self.last_taken.is_some_and(|t| now.saturating_duration_since(t) < self.every.mul_f64(0.5))
    }

    fn advance(&mut self, now: Instant) {
        self.anchor = Some(match self.anchor {
            Some(anchor) if now.saturating_duration_since(anchor) < self.every.saturating_mul(2) => anchor + self.every,
            _ => now,
        });
        self.last_taken = Some(now);
        self.deferred_at = None;
    }

    /// Returns `true` if the newly dequeued frame at `now` should be read and kept as `latest`.
    pub fn accept(&mut self, now: Instant) -> bool {
        if !self.ready(now) {
            return false;
        }
        self.advance(now);
        true
    }

    /// Records that a rejected texture is held on the GPU as the newest change so far.
    pub fn defer(&mut self, now: Instant) {
        self.deferred_at = Some(now);
    }

    #[cfg(test)]
    pub fn has_deferred(&self) -> bool {
        self.deferred_at.is_some()
    }

    /// Returns `true` if a previously deferred frame is now due and should be flushed as `latest`.
    pub fn flush_due(&mut self, now: Instant) -> bool {
        if self.deferred_at.is_none() || !self.ready(now) {
            return false;
        }
        self.advance(now);
        true
    }
}

/// A Windows monitor discovered via `EnumDisplayMonitors` + `EnumDisplayDevicesW`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinMonitor {
    /// Stable PnP device interface path (`DISPLAY_DEVICEW.DeviceID` via
    /// `EDD_GET_DEVICE_INTERFACE_NAME`), e.g. `\\?\DISPLAY#DELA0D4#...`.
    pub monitor_id: String,
    /// GDI device name (`MONITORINFOEXW.szDevice`), e.g. `\\.\DISPLAY1`.
    pub device_name: String,
}

/// Resolves which current monitor index corresponds to a saved `(saved, uuid)` pair.
///
/// Matches OBS's `duplicator-monitor-capture.c` (`DeviceID`, then `szDevice`). Returns `None`
/// when a saved `monitor_id` isn't currently connected so the watchdog waits for replug
/// instead of putting another display on air.
pub fn pick_win_monitor(saved: u32, uuid: Option<&str>, monitors: &[WinMonitor]) -> Option<u32> {
    if let Some(u) = uuid.map(str::trim).filter(|u| !u.is_empty() && !u.eq_ignore_ascii_case("DUMMY")) {
        return monitors
            .iter()
            .position(|m| {
                (!m.monitor_id.is_empty() && m.monitor_id.eq_ignore_ascii_case(u))
                    || (!m.device_name.is_empty() && m.device_name.eq_ignore_ascii_case(u))
            })
            .map(|i| i as u32);
    }
    let idx = saved as usize;
    (idx < monitors.len()).then_some(saved)
}

/// Reads OBS's Windows `monitor_capture` settings (`monitor_id` and legacy `monitor` index).
pub fn parse_obs_monitor(s: &serde_json::Value) -> (u32, Option<String>) {
    let uuid = s
        .get("monitor_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|u| !u.is_empty() && !u.eq_ignore_ascii_case("DUMMY"))
        .map(str::to_string);
    let display = match s.get("monitor").and_then(serde_json::Value::as_u64).map(|v| v as u32) {
        Some(n) => n,
        None if uuid.is_some() => u32::MAX,
        None => 0,
    };
    (display, uuid)
}

/// Returns `true` when an HRESULT from WGC (`FrameArrived` / `TryGetNextFrame` / D3D11) is a
/// real failure (negative `FAILED` HRESULT) rather than `S_OK` (empty pool returning null).
pub fn is_fatal_wgc_hresult(code: i32) -> bool {
    code < 0
}

/// Converts a positive sleep duration into a negative 100-nanosecond relative due time for Win32
/// `SetWaitableTimer`, clamping huge durations so the value never wraps positive (an absolute date).
pub fn relative_due_100ns(dur: Duration) -> Option<i64> {
    if dur.is_zero() {
        return None;
    }
    let ticks = (dur.as_nanos() / 100).clamp(1, i64::MAX as u128) as i64;
    Some(-ticks)
}

/// Strips a single trailing `.exe` suffix ASCII case-insensitively (`"GAME.EXE"` → `"GAME"`),
/// leaving earlier `.exe` segments untouched.
pub fn strip_exe_suffix(name: &str) -> &str {
    if name.len() >= 4 && name.as_bytes()[name.len() - 4..].eq_ignore_ascii_case(b".exe") {
        &name[..name.len() - 4]
    } else {
        name
    }
}

fn contains_ascii_ci(haystack: &str, needle: &str) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack.as_bytes().windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

fn ends_with_ascii_ci(haystack: &str, suffix: &str) -> bool {
    !suffix.is_empty()
        && haystack.len() >= suffix.len()
        && haystack.as_bytes()[haystack.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes())
}

fn find_dshow_device_index(devices: &[(&str, &str)], query: &str) -> Option<usize> {
    if query.is_empty() {
        return None;
    }
    if let Some(i) = devices.iter().position(|(id, _)| *id == query) {
        return Some(i);
    }
    if query.starts_with('@') {
        return devices.iter().position(|(id, _)| id.eq_ignore_ascii_case(query));
    }
    if let Some((friendly, path)) = query.split_once(":\\\\") {
        // OBS stores `Name:\\?\usb#...`; prefer matching the unique non-empty path suffix inside
        // `alt_id`, falling back to friendly name only when unambiguous with a non-empty id.
        if !path.is_empty() {
            let path_with_prefix = format!(r"\\{path}");
            if let Some(i) =
                devices.iter().position(|(id, _)| !id.is_empty() && ends_with_ascii_ci(id, &path_with_prefix))
            {
                return Some(i);
            }
            if let Some(i) = devices.iter().position(|(id, _)| !id.is_empty() && contains_ascii_ci(id, path)) {
                return Some(i);
            }
        }
        let mut matches = devices.iter().enumerate().filter(|(_, (_, name))| *name == friendly);
        if let Some((idx, (id, _))) = matches.next()
            && matches.next().is_none()
            && !id.is_empty()
        {
            return Some(idx);
        }
        return None;
    }
    devices.iter().position(|(_, name)| *name == query)
}

/// Resolves a DirectShow identifier (`@alt_id`, `"Friendly Name"`, or OBS `"Friendly:\\\\?\\path"`)
/// against enumerated `(alt_id, friendly_name)` pairs while preserving exact alt-ID matching when
/// multiple devices share a friendly name.
pub fn dshow_matches_id(devices: &[(&str, &str)], query: &str) -> bool {
    find_dshow_device_index(devices, query).is_some()
}

/// Resolves a configured camera id (`@alt_id`, OBS `"Friendly:\\\\?\\path"`, or friendly name) to
/// the enumerated DirectShow alternative name when possible so duplicate USB cameras open the
/// matching device instead of colliding on the first friendly-name match.
pub fn resolve_dshow_open_target(devices: &[(&str, &str)], query: &str) -> String {
    if query.starts_with('@') {
        return find_dshow_device_index(devices, query)
            .map(|i| devices[i].0)
            .filter(|id| !id.is_empty())
            .unwrap_or(query)
            .to_string();
    }
    if let Some((friendly, _)) = query.split_once(":\\\\") {
        return find_dshow_device_index(devices, query)
            .map(|i| devices[i].0)
            .filter(|id| !id.is_empty())
            .unwrap_or(friendly)
            .to_string();
    }
    query.to_string()
}

/// DirectShow format/codec family parsed from `ffmpeg -list_options true`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DshowFormat {
    Codec(String),
    PixelFormat(String),
}

/// One advertised capability line from DirectShow (`min s=.. fps=.. max s=.. fps=..`).
#[derive(Debug, Clone, PartialEq)]
pub struct DshowOption {
    pub pin: usize,
    pub format: DshowFormat,
    pub min_size: (u32, u32),
    pub max_size: (u32, u32),
    pub min_fps: f64,
    pub max_fps: f64,
}

/// Outcome of querying DirectShow device capabilities (`ffmpeg -list_options true`).
#[derive(Debug, Clone, PartialEq)]
pub enum DshowListStatus {
    Ok(Vec<DshowOption>),
    TimedOut,
    Missing,
    Failed,
}

/// Resolves `-list_options` status: a missing device aborts fast, whereas a slow timeout or
/// unrecognized output falls back to empty options so `dshow_attempts` still opens with defaults.
pub fn resolve_dshow_list(status: DshowListStatus) -> Result<Vec<DshowOption>, &'static str> {
    match status {
        DshowListStatus::Ok(opts) => Ok(opts),
        DshowListStatus::Missing => Err("that camera isn't connected"),
        DshowListStatus::TimedOut | DshowListStatus::Failed => Ok(Vec::new()),
    }
}

/// A chosen DirectShow input option and its nominal frame rate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DshowAttempt {
    pub args: Vec<String>,
    pub fps_hint: Option<u32>,
}

const FPS_EPS: f64 = 0.15;

impl DshowOption {
    pub fn is_mjpeg(&self) -> bool {
        matches!(&self.format, DshowFormat::Codec(c) if c.eq_ignore_ascii_case("mjpeg"))
    }

    fn format_rank(&self) -> u8 {
        match &self.format {
            DshowFormat::Codec(c) if c.eq_ignore_ascii_case("mjpeg") => 5,
            DshowFormat::PixelFormat(p) if p.eq_ignore_ascii_case("nv12") || p.eq_ignore_ascii_case("yuv420p") => 4,
            DshowFormat::PixelFormat(p) if p.eq_ignore_ascii_case("yuyv422") || p.eq_ignore_ascii_case("uyvy422") => 3,
            DshowFormat::PixelFormat(_) => 2,
            DshowFormat::Codec(_) => 1,
        }
    }

    fn covers_size(&self, w: u32, h: u32) -> bool {
        self.min_size.0 <= w && w <= self.max_size.0 && self.min_size.1 <= h && h <= self.max_size.1
    }

    fn clamped_size(&self, want: (u32, u32)) -> (u32, u32) {
        (want.0.clamp(self.min_size.0, self.max_size.0), want.1.clamp(self.min_size.1, self.max_size.1))
    }
}

fn parse_size(tok: &str) -> Option<(u32, u32)> {
    let (w, h) = tok.split_once('x')?;
    let (w, h) = (w.parse::<u32>().ok()?, h.parse::<u32>().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

/// Parses `ffmpeg -f dshow -list_options true -i video=...` stderr output into options.
pub fn parse_dshow_options(text: &str) -> Vec<DshowOption> {
    let mut out = Vec::new();
    let mut pin = 0usize;
    let mut skip_pin = false;
    for line in text.lines() {
        let l = line.split_once(']').map_or(line, |(_, r)| r).trim();
        if let Some(rest) = l.strip_prefix("Pin ") {
            pin += 1;
            let name = rest.split('"').nth(1).unwrap_or_default();
            skip_pin = name.eq_ignore_ascii_case("still");
            continue;
        }
        if skip_pin {
            continue;
        }
        let tokens: Vec<&str> = l.split_whitespace().collect();
        let format = tokens.iter().find_map(|t| {
            if let Some(c) = t.strip_prefix("vcodec=") {
                Some(DshowFormat::Codec(c.to_string()))
            } else {
                t.strip_prefix("pixel_format=").map(|p| DshowFormat::PixelFormat(p.to_string()))
            }
        });
        let Some(format) = format else { continue };
        let Some(min_i) = tokens.iter().position(|&t| t == "min") else { continue };
        let Some(max_i) = tokens.iter().position(|&t| t == "max") else { continue };
        if min_i >= max_i {
            continue;
        }
        let parse_bound = |slice: &[&str]| -> Option<((u32, u32), f64)> {
            let size = slice.iter().find_map(|t| t.strip_prefix("s=").and_then(parse_size))?;
            let fps = slice.iter().find_map(|t| t.strip_prefix("fps=").and_then(|v| v.parse::<f64>().ok()))?;
            (fps > 0.0 && fps.is_finite()).then_some((size, fps))
        };
        let Some((min_size, min_fps)) = parse_bound(&tokens[min_i + 1..max_i]) else { continue };
        let Some((max_size, max_fps)) = parse_bound(&tokens[max_i + 1..]) else { continue };
        if max_size.0 < min_size.0 || max_size.1 < min_size.1 || max_fps < min_fps {
            continue;
        }
        out.push(DshowOption { pin, format, min_size, max_size, min_fps, max_fps });
    }
    out
}

fn format_dshow_fps(want: f64, min_fps: f64, max_fps: f64) -> String {
    let fps = want.clamp(min_fps, max_fps);
    // Keep exact NTSC strings when the device reports ~59.94 / 29.97 / 23.976 so DirectShow's
    // integer 100-ns MinFrameInterval check accepts the requested frame rate.
    for (nominal, label) in [(59.94, "59.94"), (29.97, "29.97"), (23.976, "23.976")] {
        if (fps - nominal).abs() < 0.025 {
            return label.to_string();
        }
    }
    let rounded = fps.round();
    if (fps - rounded).abs() < 0.005 && rounded >= min_fps && rounded <= max_fps {
        return format!("{}", rounded as u32);
    }
    let floored = (fps * 100.0).floor() / 100.0;
    if floored >= min_fps { format!("{floored:.2}") } else { format!("{fps}") }
}

fn shaped_match(cand: (u32, u32), target: (u32, u32)) -> bool {
    let a = f64::from(cand.0) * f64::from(target.1);
    let b = f64::from(cand.1) * f64::from(target.0);
    b > 0.0 && (a / b - 1.0).abs() <= 0.02
}

/// Picks the best DirectShow option for `spec`, preferring MJPEG on high-rate/HD modes so USB2
/// capture cards (MacroSilicon MS2109/MS2130) run at 1080p60 instead of slow uncompressed YUY2.
pub fn pick_dshow_option(options: &[DshowOption], spec: &CameraSpec) -> Option<DshowAttempt> {
    if options.is_empty() {
        return None;
    }
    let default_cap = (1920u32, 1080u32);
    let target_size = spec.size.unwrap_or(default_cap);
    let target_area = u64::from(target_size.0) * u64::from(target_size.1);
    let want_fps = spec.fps.map(f64::from);

    let chosen_size_for = |opt: &DshowOption| -> (u32, u32) {
        match spec.size {
            Some((w, h)) if opt.covers_size(w, h) => (w, h),
            Some((w, h)) => opt.clamped_size((w, h)),
            None if opt.covers_size(default_cap.0, default_cap.1) => default_cap,
            None => opt.max_size,
        }
    };

    let (best_idx, _) = options.iter().enumerate().max_by(|(_, a), (_, b)| {
        let sa = chosen_size_for(a);
        let sb = chosen_size_for(b);
        let aa = u64::from(sa.0) * u64::from(sa.1);
        let ab = u64::from(sb.0) * u64::from(sb.1);

        match (spec.size, want_fps) {
            (Some(want), Some(wf)) => {
                let exact_a = sa == want;
                let exact_b = sb == want;
                let under_a = aa <= target_area;
                let under_b = ab <= target_area;
                let reach_a = a.min_fps - FPS_EPS <= wf && a.max_fps + FPS_EPS >= wf;
                let reach_b = b.min_fps - FPS_EPS <= wf && b.max_fps + FPS_EPS >= wf;
                let shape_a = shaped_match(sa, want);
                let shape_b = shaped_match(sb, want);
                let fps_a = a.max_fps.min(wf + FPS_EPS);
                let fps_b = b.max_fps.min(wf + FPS_EPS);
                exact_a
                    .cmp(&exact_b)
                    .then(under_a.cmp(&under_b))
                    .then(shape_a.cmp(&shape_b))
                    .then_with(|| if under_a { aa.cmp(&ab) } else { ab.cmp(&aa) })
                    .then(reach_a.cmp(&reach_b))
                    .then_with(|| cmp_rate_then_format(a, b, fps_a, fps_b))
            }
            (Some(want), None) => {
                let exact_a = sa == want;
                let exact_b = sb == want;
                let under_a = aa <= target_area;
                let under_b = ab <= target_area;
                let shape_a = shaped_match(sa, want);
                let shape_b = shaped_match(sb, want);
                let smooth_a = a.max_fps >= 23.5;
                let smooth_b = b.max_fps >= 23.5;
                exact_a
                    .cmp(&exact_b)
                    .then(under_a.cmp(&under_b))
                    .then(shape_a.cmp(&shape_b))
                    .then_with(|| if under_a { aa.cmp(&ab) } else { ab.cmp(&aa) })
                    .then(smooth_a.cmp(&smooth_b))
                    .then_with(|| cmp_rate_then_format(a, b, a.max_fps, b.max_fps))
            }
            (None, Some(wf)) => {
                let under_a = aa <= target_area;
                let under_b = ab <= target_area;
                let reach_a = a.min_fps - FPS_EPS <= wf && a.max_fps + FPS_EPS >= wf;
                let reach_b = b.min_fps - FPS_EPS <= wf && b.max_fps + FPS_EPS >= wf;
                let fps_a = a.max_fps.min(wf + FPS_EPS);
                let fps_b = b.max_fps.min(wf + FPS_EPS);
                under_a
                    .cmp(&under_b)
                    .then(reach_a.cmp(&reach_b))
                    .then(aa.cmp(&ab))
                    .then_with(|| cmp_rate_then_format(a, b, fps_a, fps_b))
            }
            (None, None) => {
                // "Its own" default: skip slow (<24 fps) modes when smooth ones exist, stay
                // within 1080p, prefer full HD over 720p, and prefer 60 fps + MJPEG.
                let smooth_a = a.max_fps >= 23.5;
                let smooth_b = b.max_fps >= 23.5;
                let under_a = aa <= target_area;
                let under_b = ab <= target_area;
                smooth_a
                    .cmp(&smooth_b)
                    .then(under_a.cmp(&under_b))
                    .then_with(|| if under_a { aa.cmp(&ab) } else { ab.cmp(&aa) })
                    .then_with(|| cmp_rate_then_format(a, b, a.max_fps.min(60.0), b.max_fps.min(60.0)))
            }
        }
    })?;

    let opt = &options[best_idx];
    let size = chosen_size_for(opt);
    let desired_fps = want_fps.unwrap_or(60.0).clamp(opt.min_fps, opt.max_fps);
    let fps_str = format_dshow_fps(desired_fps, opt.min_fps, opt.max_fps);
    let nominal_fps = desired_fps.round().clamp(1.0, 240.0) as u32;

    let mut args = Vec::with_capacity(6);
    match &opt.format {
        DshowFormat::Codec(c) => args.extend(["-vcodec".to_string(), c.clone()]),
        DshowFormat::PixelFormat(p) => args.extend(["-pixel_format".to_string(), p.clone()]),
    }
    args.extend(["-video_size".to_string(), format!("{}x{}", size.0, size.1), "-framerate".to_string(), fps_str]);
    Some(DshowAttempt { args, fps_hint: Some(nominal_fps) })
}

fn cmp_rate_then_format(a: &DshowOption, b: &DshowOption, rate_a: f64, rate_b: f64) -> std::cmp::Ordering {
    if (rate_a - rate_b).abs() > 0.25 {
        return rate_a.total_cmp(&rate_b);
    }
    a.format_rank().cmp(&b.format_rank()).then_with(|| rate_a.total_cmp(&rate_b)).then_with(|| b.pin.cmp(&a.pin))
}

/// Ordered DirectShow open attempts for `spec` given parsed `-list_options` entries (empty if
/// enumeration wasn't available). Always tries MJPEG + asked first, then asked, then default.
pub fn dshow_attempts(spec: &CameraSpec, options: &[DshowOption]) -> Vec<DshowAttempt> {
    let mut attempts: Vec<DshowAttempt> = Vec::new();
    let push_unique = |list: &mut Vec<DshowAttempt>, item: DshowAttempt| {
        if !list.iter().any(|existing| existing.args == item.args) {
            list.push(item);
        }
    };
    if let Some(picked) = pick_dshow_option(options, spec) {
        push_unique(&mut attempts, picked);
    }
    let asked = crate::engine::camera_args(spec);
    let allow_mjpeg_fallback = options.is_empty() || options.iter().any(DshowOption::is_mjpeg);
    if allow_mjpeg_fallback {
        let mut mjpeg = Vec::with_capacity(2 + asked.len());
        mjpeg.extend(["-vcodec".to_string(), "mjpeg".to_string()]);
        mjpeg.extend(asked.iter().cloned());
        push_unique(&mut attempts, DshowAttempt { args: mjpeg, fps_hint: spec.fps });
    }
    if !asked.is_empty() {
        push_unique(&mut attempts, DshowAttempt { args: asked, fps_hint: spec.fps });
    }
    push_unique(&mut attempts, DshowAttempt { args: Vec::new(), fps_hint: None });
    attempts
}

#[cfg(test)]
mod tests {
    use super::*;

    const MACROSILICON_MS2130: &str = r#"[dshow @ 000001a2b3c4] DirectShow video device options (from video devices)
[dshow @ 000001a2b3c4]  Pin "Capture" (alternative pin name "Capture")
[dshow @ 000001a2b3c4]   pixel_format=yuyv422  min s=1920x1080 fps=5 max s=1920x1080 fps=5
[dshow @ 000001a2b3c4]   pixel_format=yuyv422  min s=1280x720 fps=10 max s=1280x720 fps=10
[dshow @ 000001a2b3c4]   vcodec=mjpeg  min s=1920x1080 fps=5 max s=1920x1080 fps=60
[dshow @ 000001a2b3c4]   vcodec=mjpeg  min s=1280x720 fps=30 max s=1280x720 fps=60
[dshow @ 000001a2b3c4]  Pin "Still" (alternative pin name "Still")
[dshow @ 000001a2b3c4]   pixel_format=yuyv422  min s=1920x1080 fps=5 max s=1920x1080 fps=5"#;

    #[test]
    fn parses_list_options_and_ignores_still_pin() {
        let opts = parse_dshow_options(MACROSILICON_MS2130);
        assert_eq!(opts.len(), 4);
        assert_eq!(opts[0].format, DshowFormat::PixelFormat("yuyv422".into()));
        assert_eq!(opts[0].max_size, (1920, 1080));
        assert_eq!(opts[0].max_fps, 5.0);
        assert_eq!(opts[2].format, DshowFormat::Codec("mjpeg".into()));
        assert_eq!(opts[2].max_fps, 60.0);
    }

    #[test]
    fn macrosilicon_picks_mjpeg_1080p60_for_both_asked_and_default() {
        let opts = parse_dshow_options(MACROSILICON_MS2130);
        let asked = CameraSpec { id: "usb".into(), size: Some((1920, 1080)), fps: Some(60) };
        let pick = pick_dshow_option(&opts, &asked).unwrap_or_else(|| panic!("expected a choice"));
        assert_eq!(pick.args, ["-vcodec", "mjpeg", "-video_size", "1920x1080", "-framerate", "60"]);
        assert_eq!(pick.fps_hint, Some(60));

        // Even under "Its own" defaults (size: None, fps: None), it skips 5 fps YUY2 and picks 1080p60 MJPEG.
        let own = CameraSpec { id: "usb".into(), size: None, fps: None };
        let own_pick = pick_dshow_option(&opts, &own).unwrap_or_else(|| panic!("expected a choice"));
        assert_eq!(own_pick.args, ["-vcodec", "mjpeg", "-video_size", "1920x1080", "-framerate", "60"]);
    }

    #[test]
    fn lying_usb2_card_still_prefers_mjpeg_over_yuyv422() {
        let text = r#"[dshow @ 000001]  Pin "Capture"
[dshow @ 000001]   pixel_format=yuyv422  min s=1920x1080 fps=5 max s=1920x1080 fps=60
[dshow @ 000001]   vcodec=mjpeg  min s=1920x1080 fps=59.9402 max s=1920x1080 fps=59.9402"#;
        let opts = parse_dshow_options(text);
        let spec = CameraSpec { id: "usb".into(), size: Some((1920, 1080)), fps: Some(60) };
        let pick = pick_dshow_option(&opts, &spec).unwrap_or_else(|| panic!("expected a choice"));
        assert_eq!(pick.args, ["-vcodec", "mjpeg", "-video_size", "1920x1080", "-framerate", "59.94"]);
    }

    #[test]
    fn ms2109_at_max_1080p30_clamps_asked_60_to_30_instead_of_dropping_to_640x480() {
        let ms2109 = r#"[dshow @ 000001]  Pin "Capture"
[dshow @ 000001]   pixel_format=yuyv422  min s=1920x1080 fps=5 max s=1920x1080 fps=5
[dshow @ 000001]   vcodec=mjpeg  min s=1920x1080 fps=5 max s=1920x1080 fps=30
[dshow @ 000001]   vcodec=mjpeg  min s=1280x720 fps=30 max s=1280x720 fps=60"#;
        let opts = parse_dshow_options(ms2109);
        let spec = CameraSpec { id: "usb".into(), size: Some((1920, 1080)), fps: Some(60) };
        let pick = pick_dshow_option(&opts, &spec).unwrap_or_else(|| panic!("expected a choice"));
        assert_eq!(pick.args, ["-vcodec", "mjpeg", "-video_size", "1920x1080", "-framerate", "30"]);
        assert_eq!(pick.fps_hint, Some(30));
    }

    #[test]
    fn fallback_attempts_try_mjpeg_then_asked_then_none() {
        let spec = CameraSpec { id: "usb".into(), size: Some((1920, 1080)), fps: Some(60) };
        let list = dshow_attempts(&spec, &[]);
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].args, ["-vcodec", "mjpeg", "-video_size", "1920x1080", "-framerate", "60"]);
        assert_eq!(list[1].args, ["-video_size", "1920x1080", "-framerate", "60"]);
        assert!(list[2].args.is_empty());

        // YUY2-only webcam skips redundant -vcodec mjpeg fallback.
        let webcam = parse_dshow_options(
            r#"[dshow @ 000001]   pixel_format=yuyv422  min s=1280x720 fps=30 max s=1280x720 fps=30"#,
        );
        let own = CameraSpec { id: "cam".into(), size: None, fps: None };
        let cam_list = dshow_attempts(&own, &webcam);
        assert_eq!(cam_list[0].args, ["-pixel_format", "yuyv422", "-video_size", "1280x720", "-framerate", "30"]);
        assert!(cam_list.last().is_some_and(|a| a.args.is_empty()));
        assert!(!cam_list.iter().any(|a| a.args.contains(&"mjpeg".to_string())));
    }

    #[test]
    fn wgc_pacer_keeps_60hz_jittered_frames_and_throttles_higher_rates() {
        let base = Instant::now();
        let ms = |m: f64| base + Duration::from_secs_f64(m / 1000.0);

        // 60 fps canvas on a 60 Hz display with ±2 ms callback jitter (15.5–18.5 ms steps).
        let mut pacer = WgcPacer::new(60);
        let jittered_60hz = [0.0, 16.2, 33.3, 49.0, 66.7, 82.0, 100.0, 118.5, 133.3];
        let kept: usize = jittered_60hz.iter().filter(|&&t| pacer.accept(ms(t))).count();
        assert_eq!(kept, jittered_60hz.len(), "no on-time 60 Hz frame dropped");

        // 30 fps canvas on a 60 Hz display drops every other frame cleanly.
        let mut pacer30 = WgcPacer::new(30);
        let ticks_60hz: Vec<f64> = (0..10).map(|i| f64::from(i) * (1000.0 / 60.0)).collect();
        let kept30: usize = ticks_60hz.iter().filter(|&&t| pacer30.accept(ms(t))).count();
        assert_eq!(kept30, 5, "30 fps cap takes half of 60 Hz ticks");

        // 60 fps canvas on a 144 Hz display maintains ~60 fps without collapsing to 48 fps.
        let mut pacer144 = WgcPacer::new(60);
        let ticks_144hz: Vec<f64> = (0..144).map(|i| f64::from(i) * (1000.0 / 144.0)).collect();
        let kept144: usize = ticks_144hz.iter().filter(|&&t| pacer144.accept(ms(t))).count();
        assert!((58..=61).contains(&kept144), "expected ~60 fps on 144 Hz, got {kept144}");

        // Empty-pool S_OK (0) isn't fatal; DXGI_ERROR_DEVICE_REMOVED (0x887A0005 as i32) is.
        assert!(!is_fatal_wgc_hresult(0));
        assert!(is_fatal_wgc_hresult(0x887A_0005_u32 as i32));
    }

    #[test]
    fn wgc_pacer_flushes_trailing_rejected_frame_when_burst_ends() {
        let base = Instant::now();
        let ms = |m: f64| base + Duration::from_secs_f64(m / 1000.0);
        let mut pacer = WgcPacer::new(60);

        // Simulated 144 Hz burst at 60 fps canvas: frame at 0 ms accepted; 6.9 ms rejected and deferred;
        // nothing due immediately at 10 ms; flushed once the next 60 fps slot opens at 17 ms.
        assert!(pacer.accept(ms(0.0)));
        assert!(!pacer.accept(ms(6.9)));
        pacer.defer(ms(6.9));
        assert!(pacer.has_deferred());
        assert!(!pacer.flush_due(ms(10.0)), "too early before next canvas interval");
        assert!(pacer.has_deferred());
        assert!(pacer.flush_due(ms(17.0)), "final frame must flush once interval opens");
        assert!(!pacer.has_deferred());
        assert!(!pacer.flush_due(ms(50.0)), "deferred flag clears after flush");
    }

    #[test]
    fn win_monitor_matches_monitor_id_across_reorders_and_refuses_missing_id() {
        let m0 = WinMonitor {
            monitor_id: r"\\?\DISPLAY#DELA0D4#5&1234&0&UID4353#{e6f07b5f}".into(),
            device_name: r"\\.\DISPLAY1".into(),
        };
        let m1 = WinMonitor {
            monitor_id: r"\\?\DISPLAY#SAM0F10#5&9999&0&UID4354#{e6f07b5f}".into(),
            device_name: r"\\.\DISPLAY2".into(),
        };
        let two = [m0.clone(), m1.clone()];
        let reordered = [m1.clone(), m0.clone()];

        // Saved as index 0 with m0's monitor_id: after unplug/replug swaps indices, still finds m0 at index 1.
        assert_eq!(pick_win_monitor(0, Some(&m0.monitor_id), &reordered), Some(1));
        // Falls back to GDI szDevice if monitor_id fell back to szDevice.
        assert_eq!(pick_win_monitor(0, Some(r"\\.\DISPLAY1"), &reordered), Some(1));
        // Disconnected monitor_id returns None (never silently puts another display on air).
        assert_eq!(pick_win_monitor(0, Some(r"\\?\DISPLAY#GONE#0"), &reordered), None);
        // Without monitor_id: [A, B] saved 1 keeps index 1, but unplugging B -> [A] must return None
        // instead of silently putting primary display A on air.
        assert_eq!(pick_win_monitor(1, None, &two), Some(1));
        assert_eq!(pick_win_monitor(1, None, std::slice::from_ref(&m0)), None);
        assert_eq!(pick_win_monitor(5, None, &reordered), None);

        // OBS import parses monitor_id (ignoring DUMMY placeholder).
        assert_eq!(
            parse_obs_monitor(&serde_json::json!({ "monitor_id": m0.monitor_id })),
            (u32::MAX, Some(m0.monitor_id))
        );
        assert_eq!(parse_obs_monitor(&serde_json::json!({ "monitor_id": "DUMMY", "monitor": 1 })), (1, None));
    }

    #[test]
    fn list_options_timeout_falls_back_to_empty_while_missing_aborts() {
        assert_eq!(resolve_dshow_list(DshowListStatus::TimedOut), Ok(Vec::new()));
        assert_eq!(resolve_dshow_list(DshowListStatus::Failed), Ok(Vec::new()));
        assert_eq!(resolve_dshow_list(DshowListStatus::Missing), Err("that camera isn't connected"));
    }

    #[test]
    fn relative_due_100ns_handles_zero_subtick_cadence_and_overflow() {
        assert_eq!(relative_due_100ns(Duration::ZERO), None);
        assert_eq!(relative_due_100ns(Duration::from_nanos(50)), Some(-1));
        // 21.3333 ms audio block minus 1.5 ms spin margin (~19.8333 ms).
        assert_eq!(relative_due_100ns(Duration::from_nanos(19_833_333)), Some(-198_333));
        assert_eq!(relative_due_100ns(Duration::from_millis(5)), Some(-50_000));
        assert_eq!(relative_due_100ns(Duration::MAX), Some(-i64::MAX));
    }

    #[test]
    fn strip_exe_suffix_is_case_insensitive_and_strips_once() {
        assert_eq!(strip_exe_suffix("chrome.exe"), "chrome");
        assert_eq!(strip_exe_suffix("VALORANT-Win64-Shipping.EXE"), "VALORANT-Win64-Shipping");
        assert_eq!(strip_exe_suffix("Steam.Exe"), "Steam");
        assert_eq!(strip_exe_suffix("nested.exe.exe"), "nested.exe");
        assert_eq!(strip_exe_suffix("notepad"), "notepad");
        assert_eq!(strip_exe_suffix(".exe"), "");
        assert_eq!(strip_exe_suffix("ゲーム.EXE"), "ゲーム");
    }

    #[test]
    fn dshow_matches_id_resolves_alt_friendly_and_obs_paths_without_clobbering_duplicates() {
        let devs = [
            (r"@device_pnp_\\?\usb#vid_1#mi_00", "USB Camera"),
            (r"@device_pnp_\\?\usb#vid_2#mi_00", "USB Camera"),
            (r"@device_pnp_\\?\usb#vid_3#mi_00", "Sony Cam"),
        ];
        assert!(dshow_matches_id(&devs, r"@device_pnp_\\?\usb#vid_1#mi_00"));
        assert!(!dshow_matches_id(&devs, r"@device_pnp_\\?\usb#vid_9#mi_00"));
        assert!(dshow_matches_id(&devs, "USB Camera"));
        assert!(dshow_matches_id(&devs, r"USB Camera:\\?\usb#vid_2#mi_00"));
        assert!(!dshow_matches_id(&devs, r"USB Camera:\\?\usb#vid_999#mi_00"));
        assert!(dshow_matches_id(&devs, r"Sony Cam:\\?\usb#other_path"));
        assert!(!dshow_matches_id(&devs, "Missing Cam"));
    }

    #[test]
    fn dshow_matches_id_rejects_empty_path_wildcard_handles_case_and_unrelated_empty_id() {
        let devs = [
            (r"@device_pnp_\\?\usb#vid_04f2&pid_b6bf#mi_00", "USB Camera"),
            (r"@device_pnp_\\?\usb#vid_04f2&pid_b6c0#mi_00", "USB Camera"),
            (r"@device_pnp_\\?\usb#vid_1234&pid_5678#mi_00", "Sony Cam"),
            ("", "OBS Virtual Camera"),
        ];
        // Empty path after `:\\` must never act as a wildcard across devices.
        assert!(!dshow_matches_id(&devs, r"Missing Cam:\\"));
        assert!(!dshow_matches_id(&devs, r"USB Camera:\\"));
        // Windows PnP hex/prefix case differences must still match.
        assert!(dshow_matches_id(&devs, r"USB Camera:\\?\USB#VID_04F2&PID_B6C0#MI_00"));
        assert!(dshow_matches_id(&devs, r"@device_pnp_\\?\USB#VID_04F2&PID_B6BF#MI_00"));
        // An unrelated empty-alt virtual device must not block unique friendly-name fallback for Sony Cam,
        // while a unique empty-alt virtual device itself refuses ambiguous `:\\` fallback.
        assert!(dshow_matches_id(&devs, r"Sony Cam:\\?\usb#other_path"));
        assert!(!dshow_matches_id(&devs, r"OBS Virtual Camera:\\?\usb#other_path"));
        assert!(dshow_matches_id(&devs, "OBS Virtual Camera"));
    }

    #[test]
    fn resolve_dshow_open_target_selects_matching_duplicate_camera_alt_name() {
        let devs = [
            (r"@device_pnp_\\?\usb#vid_046d&pid_082d#cam1", "USB Video"),
            (r"@device_pnp_\\?\usb#vid_046d&pid_082d#cam2", "USB Video"),
            (r"@device_pnp_\\?\usb#vid_054c&pid_0001#solo", "Sony Cam"),
            ("", "OBS Virtual Camera"),
        ];
        // Second identical USB camera resolves to its own @device_pnp alternative name, never the first.
        assert_eq!(
            resolve_dshow_open_target(&devs, r"USB Video:\\?\usb#vid_046d&pid_082d#cam2"),
            r"@device_pnp_\\?\usb#vid_046d&pid_082d#cam2"
        );
        // Case-insensitive PnP match still resolves to the enumerated alt name.
        assert_eq!(
            resolve_dshow_open_target(&devs, r"USB Video:\\?\USB#VID_046D&PID_082D#CAM2"),
            r"@device_pnp_\\?\usb#vid_046d&pid_082d#cam2"
        );
        // Absent duplicate falls back cleanly to friendly name instead of an unrelated camera's alt id.
        assert_eq!(resolve_dshow_open_target(&devs, r"USB Video:\\?\usb#missing_port"), "USB Video");
        assert_eq!(resolve_dshow_open_target(&devs, r"USB Video:\\"), "USB Video");
        // Unique camera with moved USB port falls back to its own alt id even when virtual cam has empty id.
        assert_eq!(
            resolve_dshow_open_target(&devs, r"Sony Cam:\\?\usb#other_port"),
            r"@device_pnp_\\?\usb#vid_054c&pid_0001#solo"
        );
        // Unique device with empty alt id falls back to friendly name.
        assert_eq!(resolve_dshow_open_target(&devs, r"OBS Virtual Camera:\\?\usb#other"), "OBS Virtual Camera");
        assert_eq!(resolve_dshow_open_target(&devs, "USB Video"), "USB Video");
    }

    #[test]
    fn dshow_camera_cache_serves_within_ttl_reprobes_on_expiry_and_recovers_from_panic() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let start = Instant::now();
        let clock = Arc::new(Mutex::new(start));
        let cache = DshowCameraCache::new(DSHOW_CAMERA_CACHE_TTL);
        let probes = AtomicUsize::new(0);

        let now = || *clock.lock().unwrap_or_else(PoisonError::into_inner);
        let first = cache.get_or_probe(now, || {
            probes.fetch_add(1, Ordering::AcqRel);
            Some(vec!["cam-1".to_string()])
        });
        assert_eq!(first, vec!["cam-1"]);
        assert_eq!(probes.load(Ordering::Acquire), 1);

        // Within 2 s TTL: cached snapshot is returned with zero FFmpeg/probe spawns.
        *clock.lock().unwrap() = start + Duration::from_millis(1999);
        let hit = cache.get_or_probe(now, || {
            probes.fetch_add(1, Ordering::AcqRel);
            Some(vec!["cam-unexpected".to_string()])
        });
        assert_eq!(hit, vec!["cam-1"]);
        assert_eq!(probes.load(Ordering::Acquire), 1);

        // At TTL expiry: re-probes cleanly.
        *clock.lock().unwrap() = start + Duration::from_secs(2);
        let refreshed = cache.get_or_probe(now, || {
            probes.fetch_add(1, Ordering::AcqRel);
            Some(vec!["cam-2".to_string()])
        });
        assert_eq!(refreshed, vec!["cam-2"]);
        assert_eq!(probes.load(Ordering::Acquire), 2);

        // Transient probe failure invalidates cache cleanly instead of pinning stale snapshot after expiry.
        *clock.lock().unwrap() = start + Duration::from_secs(5);
        let failed = cache.get_or_probe(now, || {
            probes.fetch_add(1, Ordering::AcqRel);
            None
        });
        assert!(failed.is_empty());
        assert_eq!(probes.load(Ordering::Acquire), 3);

        // Poisoned mutex recovery: panicking inside probe still clears `in_flight` for subsequent callers.
        *clock.lock().unwrap() = start + Duration::from_secs(6);
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cache.get_or_probe(now, || panic!("simulated probe panic"));
        }));
        assert!(panicked.is_err());

        let recovered = cache.get_or_probe(now, || {
            probes.fetch_add(1, Ordering::AcqRel);
            Some(vec!["cam-recovered".to_string()])
        });
        assert_eq!(recovered, vec!["cam-recovered"]);
        assert_eq!(probes.load(Ordering::Acquire), 4);
    }
}
