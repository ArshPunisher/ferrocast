//! Support diagnostics: PII/secret redaction, likely-cause analysis, and report formatting.
//!
//! Nothing here does I/O or reads environment variables or clocks. Callers gather logs, system
//! hardware info, audio devices, stream health history, and crash dump metadata and pass them in
//! together with any known usernames or secret values so every piece of output is scrubbed.

use std::net::IpAddr;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::event::{Millis, Platform};

/// Placeholder emitted in support archives when secrets are scrubbed.
pub const REDACTED: &str = "[REDACTED]";
/// Placeholder for scrubbed email addresses.
pub const REDACTED_EMAIL: &str = "[REDACTED_EMAIL]";
/// Placeholder for scrubbed IPv4/IPv6 addresses.
pub const REDACTED_IP: &str = "[REDACTED_IP]";
/// Placeholder for scrubbed usernames inside local paths.
pub const REDACTED_USER: &str = "[REDACTED_USER]";

/// Minimum stream health drop/skip/lag ratio (`2%`) before flagging a likely cause.
const ISSUE_RATIO_PCT: f64 = 2.0;
/// Free disk warning threshold in MiB (2 GiB).
const DISK_WARN_MB: u64 = 2048;
/// Free disk critical threshold in MiB (512 MiB).
const DISK_CRITICAL_MB: u64 = 512;

/// Severity of an automatically detected likely cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CauseSeverity {
    Critical,
    Warning,
    Info,
}

impl CauseSeverity {
    pub fn label(self) -> &'static str {
        match self {
            CauseSeverity::Critical => "CRITICAL",
            CauseSeverity::Warning => "WARNING",
            CauseSeverity::Info => "INFO",
        }
    }
}

/// One automatically diagnosed cause listed at the top of the support report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LikelyCause {
    pub severity: CauseSeverity,
    pub code: String,
    pub title: String,
    pub detail: String,
    pub recommendation: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppVersionInfo {
    pub version: String,
    pub os: String,
    pub arch: String,
    pub build_mode: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsVersionInfo {
    pub name: String,
    pub version: String,
    pub long_version: String,
    pub kernel_version: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuInfo {
    pub name: String,
    pub backend: String,
    pub device_type: String,
    pub driver: String,
    pub driver_info: String,
    pub is_software: bool,
    pub active_renderer: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncoderInfo {
    pub id: String,
    pub label: String,
    pub hardware: bool,
    pub codec: String,
    pub selected: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDeviceInfo {
    /// `"input"` or `"output"`.
    pub kind: String,
    pub name: String,
    pub is_default: bool,
    pub sample_rate_hz: Option<u32>,
    pub supported_sample_rates_hz: Vec<u32>,
    pub channels: Option<u16>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioSummary {
    pub studio_sample_rate_hz: u32,
    pub monitoring_device: Option<String>,
    pub devices: Vec<AudioDeviceInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DestinationHealthSnapshot {
    pub platform: String,
    pub target: String,
    pub state: String,
    pub kbps: u32,
    pub dropped_frames: u64,
    pub total_frames: u64,
    pub rtt_ms: Option<u32>,
    pub lost_pct: Option<f32>,
    pub reconnects: u32,
    pub last_error: Option<String>,
}

/// Health snapshot of one live stream (up to the most recent 5 stored locally).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamHealthRecord {
    pub id: String,
    pub started_at: Millis,
    pub ended_at: Option<Millis>,
    pub duration_s: u64,
    pub platforms: Vec<Platform>,
    pub live: bool,
    pub unclean_end: bool,
    pub fps_target: u32,
    pub canvas: String,
    pub encoder_id: Option<String>,
    pub sample_rate_hz: u32,
    pub dropped_frames: u64,
    pub skipped_frames: u64,
    pub lagged_frames: u64,
    pub rendered_frames: u64,
    pub avg_fps: f64,
    pub cpu_pct: f32,
    pub memory_mb: u64,
    pub frame_ms: f64,
    pub destinations: Vec<DestinationHealthSnapshot>,
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrashDumpSummary {
    pub filename: String,
    pub modified_ms: Option<i64>,
    pub size_bytes: u64,
    pub kind: String,
    pub included_in_zip: bool,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorDiagnostic {
    pub platform: Platform,
    pub state: String,
    pub detail: Option<String>,
}

/// Complete unredacted diagnostic snapshot handed into [`build_support_report`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticInput {
    pub generated_at: Millis,
    pub app: AppVersionInfo,
    pub os: OsVersionInfo,
    pub gpu: GpuInfo,
    pub encoders: Vec<EncoderInfo>,
    pub configured_encoder_id: Option<String>,
    pub audio: AudioSummary,
    pub last_streams: Vec<StreamHealthRecord>,
    pub crashes: Vec<CrashDumpSummary>,
    pub connectors: Vec<ConnectorDiagnostic>,
    pub recent_log_errors: Vec<String>,
    pub logs_included: Vec<String>,
    pub disk_free_mb: Option<u64>,
    pub safe_mode: bool,
    pub unclean_exit: bool,
}

/// Top-level JSON report saved inside `report.json` in the support archive (and returned by
/// `/api/v1/diagnostics/preview`), with `likely_causes` right at the top.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SupportReport {
    pub generated_at: Millis,
    pub likely_causes: Vec<LikelyCause>,
    pub app: AppVersionInfo,
    pub os: OsVersionInfo,
    pub gpu: GpuInfo,
    pub encoders: Vec<EncoderInfo>,
    pub audio: AudioSummary,
    pub last_streams: Vec<StreamHealthRecord>,
    pub crashes: Vec<CrashDumpSummary>,
    pub connectors: Vec<ConnectorDiagnostic>,
    pub logs_included: Vec<String>,
    pub disk_free_mb: Option<u64>,
    pub safe_mode: bool,
    pub unclean_exit: bool,
}

/// Evaluates rules against gathered hardware, stream health, audio, crashes, connectors and logs,
/// then scrubs all text fields inside [`SupportReport`].
pub fn build_support_report(
    mut input: DiagnosticInput,
    known_secrets: &[&str],
    known_usernames: &[&str],
) -> SupportReport {
    input.last_streams.sort_by_key(|a| std::cmp::Reverse(a.started_at.0));
    input.last_streams.truncate(5);

    let causes = analyze_likely_causes(&input);
    let mut report = SupportReport {
        generated_at: input.generated_at,
        likely_causes: causes,
        app: input.app,
        os: input.os,
        gpu: input.gpu,
        encoders: input.encoders,
        audio: input.audio,
        last_streams: input.last_streams,
        crashes: input.crashes,
        connectors: input.connectors,
        logs_included: input.logs_included,
        disk_free_mb: input.disk_free_mb,
        safe_mode: input.safe_mode,
        unclean_exit: input.unclean_exit,
    };
    redact_report(&mut report, known_secrets, known_usernames);
    report
}

fn redact_report(report: &mut SupportReport, secrets: &[&str], users: &[&str]) {
    let sc = |s: &str| redact_text(s, secrets, users);
    for c in &mut report.likely_causes {
        c.title = sc(&c.title);
        c.detail = sc(&c.detail);
        c.recommendation = sc(&c.recommendation);
    }
    report.os.name = sc(&report.os.name);
    report.os.version = sc(&report.os.version);
    report.os.long_version = sc(&report.os.long_version);
    report.os.kernel_version = sc(&report.os.kernel_version);
    report.gpu.name = sc(&report.gpu.name);
    report.gpu.driver = sc(&report.gpu.driver);
    report.gpu.driver_info = sc(&report.gpu.driver_info);
    report.gpu.active_renderer = report.gpu.active_renderer.as_deref().map(sc);
    if let Some(m) = &mut report.audio.monitoring_device {
        *m = sc(m);
    }
    for d in &mut report.audio.devices {
        d.name = sc(&d.name);
    }
    for st in &mut report.last_streams {
        st.id = sc(&st.id);
        for d in &mut st.destinations {
            d.target = sc(&d.target);
            d.last_error = d.last_error.as_deref().map(sc);
        }
    }
    for cr in &mut report.crashes {
        cr.filename = sc(&cr.filename);
        cr.summary = cr.summary.as_deref().map(sc);
    }
    for cn in &mut report.connectors {
        cn.detail = cn.detail.as_deref().map(sc);
    }
    for log_name in &mut report.logs_included {
        *log_name = sc(log_name);
    }
}

/// Pure rule-based analyzer producing ranked [`LikelyCause`] entries from diagnostic telemetry.
pub fn analyze_likely_causes(input: &DiagnosticInput) -> Vec<LikelyCause> {
    let mut causes = Vec::new();

    if !input.crashes.is_empty() {
        let newest = &input.crashes[0].filename;
        causes.push(LikelyCause {
            severity: CauseSeverity::Critical,
            code: "crash_dumps_present".into(),
            title: format!("{} crash dump(s) found on disk", input.crashes.len()),
            detail: format!("Most recent crash artifact: {newest}"),
            recommendation: "Share this archive with support and test in Safe Mode (Help > Restart in Safe Mode) if crashes continue.".into(),
        });
    }

    if input.unclean_exit || input.safe_mode {
        causes.push(LikelyCause {
            severity: CauseSeverity::Warning,
            code: "unclean_exit_safe_mode".into(),
            title: if input.safe_mode {
                "Ferrocast is running in Safe Mode".into()
            } else {
                "Previous session exited unexpectedly".into()
            },
            detail: if input.unclean_exit {
                "Startup detected a leftover running marker from an unclean shutdown.".into()
            } else {
                "Third-party plugins and VST3 filters are held off in Safe Mode.".into()
            },
            recommendation: "Try streaming with Safe Mode first to rule out a third-party plugin or capture filter."
                .into(),
        });
    }

    if input.gpu.name.trim().is_empty() || input.gpu.is_software {
        let gpu_desc = if input.gpu.name.trim().is_empty() {
            "No hardware graphics adapter reported".to_string()
        } else {
            format!("Adapter '{}' is running in software mode ({})", input.gpu.name, input.gpu.device_type)
        };
        causes.push(LikelyCause {
            severity: CauseSeverity::Critical,
            code: "gpu_software_fallback".into(),
            title: "Hardware GPU compositing is unavailable".into(),
            detail: gpu_desc,
            recommendation: "Update your graphics driver or switch to your dedicated GPU for smooth frame compositing."
                .into(),
        });
    }

    if let Some(free) = input.disk_free_mb {
        if free < DISK_CRITICAL_MB {
            causes.push(LikelyCause {
                severity: CauseSeverity::Critical,
                code: "disk_space_low".into(),
                title: format!("Critically low free disk space ({free} MB left)"),
                detail: "Recordings and crash dumps can fail when free space falls below 512 MB.".into(),
                recommendation: "Free up disk space or move your recording folder to another drive.".into(),
            });
        } else if free < DISK_WARN_MB {
            causes.push(LikelyCause {
                severity: CauseSeverity::Warning,
                code: "disk_space_low".into(),
                title: format!("Low free disk space ({free} MB left)"),
                detail: "Less than 2 GB remains on the data/recording drive.".into(),
                recommendation: "Clean up older recordings before long streams.".into(),
            });
        }
    }

    check_stream_health_causes(input, &mut causes);
    check_encoder_and_audio_causes(input, &mut causes);
    check_connector_and_log_causes(input, &mut causes);

    causes.sort_by(|a, b| a.severity.cmp(&b.severity).then(a.code.cmp(&b.code)));
    causes
}

fn pct(part: u64, total: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    (part as f64) * 100.0 / (total as f64)
}

fn check_stream_health_causes(input: &DiagnosticInput, out: &mut Vec<LikelyCause>) {
    let mut worst_drop_pct = 0.0_f64;
    let mut max_reconnects = 0_u32;
    let mut failed_dest: Option<String> = None;
    let mut worst_skip_pct = 0.0_f64;
    let mut worst_lag_pct = 0.0_f64;
    let mut slow_frame: Option<(f64, f64)> = None;

    for s in input.last_streams.iter().take(5) {
        let dest_sent: u64 = s.destinations.iter().map(|d| d.total_frames).sum();
        let dest_dropped: u64 = s.destinations.iter().map(|d| d.dropped_frames).sum();
        let total_net = dest_sent.max(s.rendered_frames);
        let dropped = s.dropped_frames.max(dest_dropped);
        worst_drop_pct = worst_drop_pct.max(pct(dropped, total_net));
        worst_skip_pct = worst_skip_pct.max(pct(s.skipped_frames, s.rendered_frames));
        worst_lag_pct = worst_lag_pct.max(pct(s.lagged_frames, s.rendered_frames));

        for d in &s.destinations {
            max_reconnects = max_reconnects.max(d.reconnects);
            if d.state == "failed" && failed_dest.is_none() {
                failed_dest = Some(format!("{}: {}", d.platform, d.last_error.as_deref().unwrap_or("stream failed")));
            }
        }
        if s.fps_target > 0 {
            let budget_ms = 1000.0 / f64::from(s.fps_target);
            if s.frame_ms > budget_ms * 1.15 {
                let replace = slow_frame.is_none_or(|(prev, _)| s.frame_ms > prev);
                if replace {
                    slow_frame = Some((s.frame_ms, budget_ms));
                }
            }
        }
    }

    if worst_drop_pct >= ISSUE_RATIO_PCT || max_reconnects > 0 || failed_dest.is_some() {
        let mut parts = Vec::new();
        if worst_drop_pct >= ISSUE_RATIO_PCT {
            parts.push(format!("{worst_drop_pct:.1}% network frame drops"));
        }
        if max_reconnects > 0 {
            parts.push(format!("{max_reconnects} reconnect(s)"));
        }
        if let Some(fail) = failed_dest {
            parts.push(fail);
        }
        out.push(LikelyCause {
            severity: if worst_drop_pct >= 10.0 {
                CauseSeverity::Critical
            } else {
                CauseSeverity::Warning
            },
            code: "stream_network_drops".into(),
            title: "Network instability or dropped frames in recent stream(s)".into(),
            detail: parts.join("; "),
            recommendation: "Use wired Ethernet if possible, lower video bitrate, or turn on Auto Quality / Upload protection in Go live.".into(),
        });
    }

    if worst_skip_pct >= ISSUE_RATIO_PCT {
        out.push(LikelyCause {
            severity: if worst_skip_pct >= 10.0 { CauseSeverity::Critical } else { CauseSeverity::Warning },
            code: "stream_encoder_overload".into(),
            title: format!("Video encoder skipped {worst_skip_pct:.1}% of frames in a recent stream"),
            detail: "The encoder fell behind real time while encoding output frames.".into(),
            recommendation:
                "Prefer a hardware encoder (VideoToolbox/NVENC/AMF/QSV) or reduce output resolution / frame rate."
                    .into(),
        });
    }

    if worst_lag_pct >= ISSUE_RATIO_PCT || slow_frame.is_some() {
        let detail = if let Some((ms, budget)) = slow_frame {
            format!(
                "Render lag reached {worst_lag_pct:.1}% and frame draw time hit {ms:.1} ms (target budget {budget:.1} ms)."
            )
        } else {
            format!("Compositor lagged {worst_lag_pct:.1}% of frames in a recent stream.")
        };
        out.push(LikelyCause {
            severity: CauseSeverity::Warning,
            code: "stream_render_lag".into(),
            title: "GPU compositor lagged behind canvas frame rate".into(),
            detail,
            recommendation: "Cap game frame rate below GPU saturation or trim heavy animated browser/filter sources."
                .into(),
        });
    }
}

fn check_encoder_and_audio_causes(input: &DiagnosticInput, out: &mut Vec<LikelyCause>) {
    let has_hw = input.encoders.iter().any(|e| e.hardware);
    let selected = input
        .configured_encoder_id
        .as_deref()
        .or_else(|| input.encoders.iter().find(|e| e.selected).map(|e| e.id.as_str()));

    if let Some(id) = selected
        && id == "obs_x264"
        && has_hw
    {
        out.push(LikelyCause {
            severity: CauseSeverity::Info,
            code: "software_encoder_selected".into(),
            title: "Using software (CPU) x264 encoder while hardware encoding is available".into(),
            detail: "CPU software encoding increases thermal/CPU load compared to GPU hardware encoders.".into(),
            recommendation:
                "Switch the stream video encoder to Hardware in Studio Settings > Output unless x264 is intentional."
                    .into(),
        });
    } else if !input.encoders.is_empty() && !has_hw {
        out.push(LikelyCause {
            severity: CauseSeverity::Warning,
            code: "no_hardware_encoder".into(),
            title: "No hardware video encoder detected".into(),
            detail: "Only CPU software encoding (x264) is currently listed.".into(),
            recommendation: "Verify GPU drivers are installed so hardware encoding stays available.".into(),
        });
    }

    let rate = input.audio.studio_sample_rate_hz;
    if rate > 0 {
        let mismatches: Vec<String> = input
            .audio
            .devices
            .iter()
            .filter_map(|d| {
                let r = d.sample_rate_hz?;
                (r != rate).then(|| format!("{} ({} at {r} Hz)", d.name, d.kind))
            })
            .take(4)
            .collect();
        if !mismatches.is_empty() {
            out.push(LikelyCause {
                severity: CauseSeverity::Warning,
                code: "audio_sample_rate_mismatch".into(),
                title: format!("Audio device sample rate differs from Ferrocast's {rate} Hz mix"),
                detail: format!("Mismatched devices: {}", mismatches.join(", ")),
                recommendation: "Set OS audio devices and Studio Settings > Audio to the same sample rate (usually 48000 Hz) to avoid clock drift.".into(),
            });
        }
    }

    if !input.audio.devices.iter().any(|d| d.kind == "input") {
        out.push(LikelyCause {
            severity: CauseSeverity::Info,
            code: "no_audio_input".into(),
            title: "No microphone / audio input devices detected".into(),
            detail: "Audio input device enumeration returned no active capture hardware.".into(),
            recommendation: "Check microphone USB connection or OS Microphone privacy permissions.".into(),
        });
    }
}

fn check_connector_and_log_causes(input: &DiagnosticInput, out: &mut Vec<LikelyCause>) {
    let broken: Vec<String> = input
        .connectors
        .iter()
        .filter(|c| matches!(c.state.as_str(), "sign_in_needed" | "scopes_missing" | "error" | "reconnecting"))
        .map(|c| match &c.detail {
            Some(d) => format!("{} ({}: {d})", c.platform.label(), c.state),
            None => format!("{} ({})", c.platform.label(), c.state),
        })
        .collect();
    if !broken.is_empty() {
        out.push(LikelyCause {
            severity: CauseSeverity::Warning,
            code: "connector_auth_issue".into(),
            title: "One or more platform connections need attention".into(),
            detail: broken.join("; "),
            recommendation: "Reconnect affected platform accounts in Settings > Accounts.".into(),
        });
    }

    if !input.recent_log_errors.is_empty() {
        let sample = input.recent_log_errors.iter().rev().take(3).cloned().collect::<Vec<_>>().join(" | ");
        out.push(LikelyCause {
            severity: CauseSeverity::Warning,
            code: "recent_log_errors".into(),
            title: format!("{} error line(s) found in recent application logs", input.recent_log_errors.len()),
            detail: truncate_chars(&sample, 240),
            recommendation: "Inspect logs/ferrocast.log inside this archive for full timestamps.".into(),
        });
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Renders a plain-text human summary (`SUMMARY.txt`) starting with `=== LIKELY CAUSES ===`.
pub fn format_summary_txt(report: &SupportReport, secrets: &[&str], usernames: &[&str]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "FERROCAST SUPPORT REPORT");
    let _ = writeln!(s, "Generated (unix ms): {}", report.generated_at.0);
    let _ = writeln!(s);

    let _ = writeln!(s, "=== LIKELY CAUSES ===");
    if report.likely_causes.is_empty() {
        let _ = writeln!(s, "No obvious issues detected across recent streams, encoders, audio, and logs.");
    } else {
        for (idx, c) in report.likely_causes.iter().enumerate() {
            let _ = writeln!(s, "{}. [{}] {} ({})", idx + 1, c.severity.label(), c.title, c.code);
            let _ = writeln!(s, "   Detail: {}", c.detail);
            let _ = writeln!(s, "   Action: {}", c.recommendation);
        }
    }
    let _ = writeln!(s);

    let _ = writeln!(s, "=== APP & OS VERSIONS ===");
    let _ = writeln!(
        s,
        "App: Ferrocast {} ({}-{}, {})",
        report.app.version, report.app.os, report.app.arch, report.app.build_mode
    );
    let _ = writeln!(
        s,
        "OS: {} {} ({}) | Kernel: {}",
        report.os.name, report.os.version, report.os.long_version, report.os.kernel_version
    );
    let _ = writeln!(
        s,
        "Safe mode: {} | Unclean previous exit: {} | Free disk: {}",
        report.safe_mode,
        report.unclean_exit,
        report.disk_free_mb.map_or_else(|| "unknown".into(), |mb| format!("{mb} MB"))
    );
    let _ = writeln!(s);

    let _ = writeln!(s, "=== GPU & ENCODERS ===");
    let _ = writeln!(
        s,
        "GPU: {} | Backend: {} | Type: {} | Software: {}",
        if report.gpu.name.is_empty() { "unavailable" } else { &report.gpu.name },
        report.gpu.backend,
        report.gpu.device_type,
        report.gpu.is_software
    );
    if !report.gpu.driver.is_empty() || !report.gpu.driver_info.is_empty() {
        let _ = writeln!(s, "GPU Driver: {} {}", report.gpu.driver, report.gpu.driver_info);
    }
    if let Some(r) = &report.gpu.active_renderer {
        let _ = writeln!(s, "Studio Renderer: {r}");
    }
    if report.encoders.is_empty() {
        let _ = writeln!(s, "Encoders: none detected");
    } else {
        for e in &report.encoders {
            let mark = if e.selected { " [SELECTED]" } else { "" };
            let hw = if e.hardware { "hardware" } else { "software" };
            let _ = writeln!(s, "- {} ({}, {}, {}){mark}", e.label, e.id, e.codec, hw);
        }
    }
    let _ = writeln!(s);

    let _ = writeln!(s, "=== AUDIO DEVICES & SAMPLE RATES ===");
    let _ = writeln!(
        s,
        "Studio sample rate: {} Hz | Monitoring: {}",
        report.audio.studio_sample_rate_hz,
        report.audio.monitoring_device.as_deref().unwrap_or("default")
    );
    if report.audio.devices.is_empty() {
        let _ = writeln!(s, "Audio devices: none reported");
    } else {
        for d in &report.audio.devices {
            let def = if d.is_default { " [default]" } else { "" };
            let sr = d.sample_rate_hz.map_or_else(|| "unknown Hz".into(), |hz| format!("{hz} Hz"));
            let ch = d.channels.map_or_else(String::new, |c| format!(", {c} ch"));
            let sup = if d.supported_sample_rates_hz.is_empty() {
                String::new()
            } else {
                let list: Vec<String> = d.supported_sample_rates_hz.iter().map(|r| r.to_string()).collect();
                format!(" [supported: {} Hz]", list.join(", "))
            };
            let _ = writeln!(s, "- [{}] {}{def}: {sr}{ch}{sup}", d.kind, d.name);
        }
    }
    let _ = writeln!(s);

    let _ = writeln!(s, "=== LAST 5 STREAMS HEALTH ===");
    if report.last_streams.is_empty() {
        let _ = writeln!(s, "No stream health records recorded yet.");
    } else {
        for (i, st) in report.last_streams.iter().enumerate() {
            let plats: Vec<&str> = st.platforms.iter().map(|p| p.label()).collect();
            let _ = writeln!(
                s,
                "{}. Stream {} (started {}, duration {}s, live={}, unclean_end={}) [{}]",
                i + 1,
                st.id,
                st.started_at.0,
                st.duration_s,
                st.live,
                st.unclean_end,
                plats.join(", ")
            );
            let _ = writeln!(
                s,
                "   Video: {}@{}fps encoder={} frame_ms={:.1} avg_fps={:.1} | dropped={} skipped={} lagged={}/{} | CPU {:.1}% RAM {}MB",
                st.canvas,
                st.fps_target,
                st.encoder_id.as_deref().unwrap_or("unknown"),
                st.frame_ms,
                st.avg_fps,
                st.dropped_frames,
                st.skipped_frames,
                st.lagged_frames,
                st.rendered_frames,
                st.cpu_pct,
                st.memory_mb
            );
            for d in &st.destinations {
                let _ = writeln!(
                    s,
                    "   Dest {} ({}): state={} {} kbps dropped={}/{} reconnects={}{}",
                    d.platform,
                    d.target,
                    d.state,
                    d.kbps,
                    d.dropped_frames,
                    d.total_frames,
                    d.reconnects,
                    d.last_error.as_deref().map(|e| format!(" error={e}")).unwrap_or_default()
                );
            }
        }
    }
    let _ = writeln!(s);

    let _ = writeln!(s, "=== CRASH DUMPS ===");
    if report.crashes.is_empty() {
        let _ = writeln!(s, "No crash dumps present.");
    } else {
        for cr in &report.crashes {
            let _ = writeln!(
                s,
                "- {} ({} bytes, kind={}, included={})",
                cr.filename, cr.size_bytes, cr.kind, cr.included_in_zip
            );
        }
    }
    let _ = writeln!(s);

    let _ = writeln!(s, "=== LOG FILES INCLUDED ===");
    if report.logs_included.is_empty() {
        let _ = writeln!(s, "None");
    } else {
        for name in &report.logs_included {
            let _ = writeln!(s, "- {name}");
        }
    }

    redact_text(&s, secrets, usernames)
}

// ========================================================================= Redaction

/// Controls placeholder tokens used when redacting text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactStyle {
    /// Human/archive clear placeholders (`[REDACTED]`, `[REDACTED_EMAIL]`, `[REDACTED_IP]`, `[REDACTED_USER]`).
    Report,
    /// Compact studio log viewer placeholders (`•••`), matching legacy `redact_log_line` contract.
    Bullets,
}

/// Redacts all secrets, stream keys, tokens, passwords, emails, IPv4/IPv6 addresses, and
/// OS usernames in paths from `text`.
pub fn redact_text(text: &str, known_secrets: &[&str], known_usernames: &[&str]) -> String {
    redact_with(text, known_secrets, known_usernames, RedactStyle::Report)
}

/// Compact log-line redaction using `•••` placeholders while scrubbing keys, tokens, passwords,
/// emails, IPs, and usernames in paths.
pub fn redact_log_line(line: &str) -> String {
    redact_with(line, &[], &[], RedactStyle::Bullets)
}

/// Core configurable redactor. Safe against arbitrary UTF-8 input.
pub fn redact_with(text: &str, known_secrets: &[&str], known_usernames: &[&str], style: RedactStyle) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mask = match style {
        RedactStyle::Report => REDACTED,
        RedactStyle::Bullets => "•••",
    };
    let mask_email = match style {
        RedactStyle::Report => REDACTED_EMAIL,
        RedactStyle::Bullets => "•••",
    };
    let mask_ip = match style {
        RedactStyle::Report => REDACTED_IP,
        RedactStyle::Bullets => "•••",
    };
    let mask_user = match style {
        RedactStyle::Report => REDACTED_USER,
        RedactStyle::Bullets => "•••",
    };

    let mut out = text.to_string();

    // 1. Explicit fixture/configured secrets (>= 4 chars, longest first so substrings don't shadow).
    let mut secrets: Vec<&str> = known_secrets.iter().copied().filter(|s| s.len() >= 4).collect();
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    for secret in secrets {
        if out.contains(secret) {
            out = out.replace(secret, mask);
        }
    }

    // 2. Path usernames (`/Users/<u`, `/home/<u`, `C:\Users\<u`, `C:/Users/<u`, `\\Users\<u`)
    out = scrub_path_usernames(&out, mask_user);

    // 3. Explicit known usernames on identifier/word boundaries (caught in paths, hostnames, mics).
    let mut users: Vec<&str> = known_usernames.iter().copied().filter(|u| u.len() >= 3).collect();
    users.sort_by_key(|u| std::cmp::Reverse(u.len()));
    for user in users {
        out = replace_identifier_ci(&out, user, mask_user);
    }

    // 4. URLs with userinfo (`scheme://user:pass@host`) or streaming keys (`rtmp://.../app/<key>`).
    out = scrub_urls(&out, mask);

    // 5. Structured secret key-value and JSON fields (`key=`, `token=`, `"password":"..."`, etc.).
    out = scrub_kv_and_json_secrets(&out, mask);

    // 6. Token prefixes (`Bearer `, `Basic `, `live_`, `oauth:`, JWT `eyJ...`, API prefixes) & secret words.
    out = scrub_token_words(&out, mask);

    // 7. Emails (`local@domain.tld`).
    out = scrub_emails(&out, mask_email);

    // 8. IPv4 and IPv6 literals (`192.168.1.42`, `[fe80::1]:47820`, `2001:db8::1%en0`, etc.).
    out = scrub_ip_addresses(&out, mask_ip);

    out
}

/// Scrubs both ASCII and UTF-16LE representations of secrets, emails, IPs, and path usernames
/// in-place inside a raw binary buffer (such as a Windows `.dmp` minidump), overwriting matched
/// sensitive bytes with `*` (`0x2a` ASCII or `0x2a 0x00` UTF-16LE) to preserve fixed offsets.
pub fn redact_bytes_in_place(buf: &mut [u8], known_secrets: &[&str], known_usernames: &[&str]) {
    if buf.is_empty() {
        return;
    }

    // Helper: overwrite every occurrence of needle (exact + lowercase) with `*`.
    let scrub_ascii_needle = |data: &mut [u8], needle: &[u8]| {
        if needle.is_empty() || needle.len() > data.len() {
            return;
        }
        let mut i = 0;
        while i + needle.len() <= data.len() {
            if data[i..i + needle.len()].eq_ignore_ascii_case(needle) {
                data[i..i + needle.len()].fill(b'*');
                i += needle.len();
            } else {
                i += 1;
            }
        }
    };

    let scrub_utf16le_needle = |data: &mut [u8], s: &str| {
        let units: Vec<u16> = s.encode_utf16().collect();
        let blen = units.len() * 2;
        if blen == 0 || blen > data.len() {
            return;
        }
        let pattern: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
        let lower_pattern: Vec<u8> = s.to_lowercase().encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let mut i = 0;
        while i + blen <= data.len() {
            let slice = &data[i..i + blen];
            if slice == pattern.as_slice()
                || (lower_pattern.len() == blen && slice.eq_ignore_ascii_case_utf16(&lower_pattern))
            {
                for j in (0..blen).step_by(2) {
                    data[i + j] = b'*';
                    data[i + j + 1] = 0;
                }
                i += blen;
            } else {
                i += 1;
            }
        }
    };

    for s in known_secrets.iter().copied().filter(|s| s.len() >= 4) {
        scrub_ascii_needle(buf, s.as_bytes());
        scrub_utf16le_needle(buf, s);
    }
    for u in known_usernames.iter().copied().filter(|u| u.len() >= 3) {
        scrub_ascii_needle(buf, u.as_bytes());
        scrub_utf16le_needle(buf, u);
    }

    // Generic ASCII runs (emails, IPs, path usernames, key=val, Bearer tokens, live_, eyJ...).
    let mut i = 0;
    while i < buf.len() {
        if is_printable_ascii_text(buf[i]) {
            let start = i;
            while i < buf.len() && is_printable_ascii_text(buf[i]) {
                i += 1;
            }
            if i - start >= 6 {
                let s = String::from_utf8_lossy(&buf[start..i]).into_owned();
                let red = redact_text(&s, known_secrets, known_usernames);
                if red != s {
                    scrub_ascii_span(&mut buf[start..i], &s);
                }
            }
        } else {
            i += 1;
        }
    }

    // Generic UTF-16LE runs (Windows minidumps store environment blocks & file paths in UTF-16LE).
    for offset in [0usize, 1usize] {
        let mut idx = offset;
        while idx + 1 < buf.len() {
            if is_utf16le_printable_ascii(buf[idx], buf[idx + 1]) {
                let start = idx;
                let mut text = String::new();
                while idx + 1 < buf.len() && is_utf16le_printable_ascii(buf[idx], buf[idx + 1]) {
                    text.push(buf[idx] as char);
                    idx += 2;
                }
                if text.len() >= 6 {
                    let red = redact_text(&text, known_secrets, known_usernames);
                    if red != text {
                        scrub_utf16le_span(&mut buf[start..idx], &text, known_secrets, known_usernames);
                    }
                }
            } else {
                idx += 2;
            }
        }
    }
}

trait EqUtf16AsciiCi {
    fn eq_ignore_ascii_case_utf16(&self, other: &[u8]) -> bool;
}

impl EqUtf16AsciiCi for [u8] {
    fn eq_ignore_ascii_case_utf16(&self, other: &[u8]) -> bool {
        if self.len() != other.len() || !self.len().is_multiple_of(2) {
            return false;
        }
        self.as_chunks::<2>()
            .0
            .iter()
            .zip(other.as_chunks::<2>().0.iter())
            .all(|(a, b)| a[1] == b[1] && (a[1] != 0 || a[0].eq_ignore_ascii_case(&b[0])))
    }
}

fn is_printable_ascii_text(b: u8) -> bool {
    (0x20..=0x7e).contains(&b)
}

fn is_utf16le_printable_ascii(lo: u8, hi: u8) -> bool {
    hi == 0 && (0x20..=0x7e).contains(&lo)
}

/// Masks tokens inside an ASCII run in place using byte length preservation where detected.
fn scrub_ascii_span(bytes: &mut [u8], s: &str) {
    for (a, b) in sensitive_byte_ranges(s) {
        if a < b && b <= bytes.len() {
            bytes[a..b].fill(b'*');
        }
    }
}

fn scrub_utf16le_span(bytes: &mut [u8], s: &str, secrets: &[&str], users: &[&str]) {
    let mut ranges = sensitive_byte_ranges(s);
    for pat in secrets.iter().chain(users.iter()).filter(|x| x.len() >= 3) {
        let mut pos = 0;
        let low_s = s.to_ascii_lowercase();
        let low_p = pat.to_ascii_lowercase();
        while let Some(found) = low_s[pos..].find(&low_p) {
            ranges.push((pos + found, pos + found + pat.len()));
            pos += found + pat.len();
        }
    }
    for (a, b) in ranges {
        for char_i in a..b {
            let byte_i = char_i * 2;
            if byte_i + 1 < bytes.len() {
                bytes[byte_i] = b'*';
                bytes[byte_i + 1] = 0;
            }
        }
    }
}

/// Identifies `(start, end)` byte offsets of sensitive spans inside pure-ASCII `s` for fixed-width
/// binary buffer scrubbing.
fn sensitive_byte_ranges(s: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let lower = s.to_ascii_lowercase();

    // Path usernames
    for prefix in ["/users/", "/home/", "\\users\\", "/Users/", "/home/", "\\Users\\"] {
        let mut pos = 0;
        let low_pref = prefix.to_ascii_lowercase();
        while let Some(idx) = lower[pos..].find(&low_pref) {
            let u_start = pos + idx + low_pref.len();
            let u_end = s[u_start..]
                .find(['/', '\\', ' ', '\t', '\r', '\n', '"', '\'', ',', ';'])
                .map_or(s.len(), |n| u_start + n);
            if u_end > u_start {
                ranges.push((u_start, u_end));
            }
            pos = u_end.max(u_start + 1);
        }
    }

    // Stream URLs & KV secrets
    for marker in [
        "key=",
        "stream_key=",
        "streamkey=",
        "token=",
        "access_token=",
        "refresh_token=",
        "id_token=",
        "passphrase=",
        "streamid=",
        "password=",
        "passwd=",
        "secret=",
        "client_secret=",
        "apikey=",
        "api_key=",
        "pass=",
        "auth=",
        "authorization=",
        "webhook=",
        "bearer=",
        "bearer:",
        "bearer ",
        "basic ",
        "oauth:",
        "live_",
        "sk_live_",
        "whsec_",
        "ghp_",
        "aiza",
        "xoxb-",
        "eyj",
    ] {
        let mut pos = 0;
        while let Some(idx) = lower[pos..].find(marker) {
            let v_start = pos + idx + marker.len();
            let v_end = s[v_start..]
                .find(|c: char| c.is_ascii_whitespace() || matches!(c, '&' | '"' | '\'' | ',' | ';' | ')'))
                .map_or(s.len(), |n| v_start + n);
            if marker.starts_with("eyj")
                || marker == "live_"
                || marker == "oauth:"
                || marker == "ghp_"
                || marker == "sk_live_"
                || marker == "whsec_"
                || marker == "aiza"
                || marker == "xoxb-"
            {
                if v_end > pos + idx {
                    ranges.push((pos + idx, v_end));
                }
            } else if v_end > v_start {
                ranges.push((v_start, v_end));
            }
            pos = v_end.max(v_start + 1);
        }
    }

    // Emails & IPv4/IPv6 tokens & rtmp trailing path
    for (idx, _) in s.match_indices('@') {
        let a = s[..idx]
            .rfind(|c: char| !matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '%' | '+' | '-'))
            .map_or(0, |p| p + 1);
        let b = s[idx + 1..]
            .find(|c: char| !matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '-'))
            .map_or(s.len(), |p| idx + 1 + p);
        if a < idx && idx + 1 < b && s[idx + 1..b].contains('.') {
            ranges.push((a, b));
        }
    }

    for (start, end, _) in find_ip_spans(s) {
        ranges.push((start, end));
    }

    for scheme in ["rtmp://", "rtmps://", "srt://", "rist://", "whip://"] {
        let mut pos = 0;
        while let Some(idx) = lower[pos..].find(scheme) {
            let rest = &s[pos + idx..];
            let url_end = rest
                .find(|c: char| c.is_ascii_whitespace() || matches!(c, '"' | '\'' | ',' | ')' | '>' | '<'))
                .unwrap_or(rest.len());
            if let Some(slash) = rest[..url_end].rfind('/')
                && slash > scheme.len()
                && slash + 1 < url_end
            {
                ranges.push((pos + idx + slash + 1, pos + idx + url_end));
            }
            pos += idx + url_end.max(1);
        }
    }

    ranges
}

fn replace_identifier_ci(text: &str, needle: &str, replacement: &str) -> String {
    let low_text = text.to_lowercase();
    let low_needle = needle.to_lowercase();
    if low_text.len() != text.len() || low_needle.is_empty() {
        // Fallback if case folding changed byte length.
        return text.replace(needle, replacement);
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(rel) = low_text[i..].find(&low_needle) {
        let start = i + rel;
        let end = start + needle.len();
        let left_ok = start == 0 || !is_ident_char(bytes[start - 1]);
        let right_ok = end == bytes.len() || !is_ident_char(bytes[end]) || text[end..].starts_with("'s");
        out.push_str(&text[i..start]);
        if left_ok && right_ok {
            out.push_str(replacement);
        } else {
            out.push_str(&text[start..end]);
        }
        i = end;
    }
    out.push_str(&text[i..]);
    out
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn scrub_path_usernames(text: &str, mask: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let lbytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let matched = ["/users/", "/home/", "\\users\\", "/users\\", "\\users/"]
            .into_iter()
            .find(|p| lbytes[i..].starts_with(p.as_bytes()));

        if let Some(pref) = matched {
            // Check preceding context: start of line/quote/space, drive `C:`, or path boundary.
            let prev_ok = i == 0
                || bytes[i - 1] == b':'
                || bytes[i - 1].is_ascii_whitespace()
                || matches!(bytes[i - 1], b'"' | b'\'' | b'(' | b'[' | b'=' | b'<' | b'\\' | b'/' | b',');
            if prev_ok {
                let u_start = i + pref.len();
                let u_end = find_path_segment_end(text, u_start);
                if u_end > u_start {
                    let seg = &text[u_start..u_end];
                    // Don't double-redact an already masked segment or OS system pseudo-dirs.
                    if !matches!(seg, "Public" | "Shared" | "Default" | "All Users" | "Default User") && seg != mask {
                        out.push_str(&text[i..u_start]);
                        out.push_str(mask);
                        i = u_end;
                        continue;
                    }
                }
            }
        }
        let ch = text[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn find_path_segment_end(text: &str, start: usize) -> usize {
    let mut end = start;
    for (rel, ch) in text[start..].char_indices() {
        if matches!(
            ch,
            '/' | '\\' | ' ' | '\t' | '\r' | '\n' | '"' | '\'' | '`' | ',' | ';' | ')' | ']' | '>' | '?' | '&'
        ) {
            break;
        }
        end = start + rel + ch.len_utf8();
    }
    end
}

fn scrub_urls(text: &str, mask: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let lower = text.to_ascii_lowercase();
    while let Some(rel) = lower[i..].find("://") {
        let scheme_end = i + rel + 3;
        out.push_str(&text[i..scheme_end]);
        let rest = &text[scheme_end..];
        let url_len = rest
            .find(|c: char| c.is_ascii_whitespace() || matches!(c, '"' | '\'' | ')' | '>' | '<' | '`'))
            .unwrap_or(rest.len());
        let url_body = &rest[..url_len];
        out.push_str(&scrub_url_body(&lower[i..scheme_end], url_body, mask));
        i = scheme_end + url_len;
    }
    out.push_str(&text[i..]);
    out
}

fn scrub_url_body(scheme_prefix: &str, body: &str, mask: &str) -> String {
    let mut s = body.to_string();
    // 1. Userinfo (`user:pass@host`) in any URL authority.
    let auth_end = s.find(['/', '?', '#']).unwrap_or(s.len());
    if let Some(at) = s[..auth_end].rfind('@') {
        s = format!("{mask}{}", &s[at..]);
    }
    // 2. Discord / Slack webhooks.
    let low = s.to_ascii_lowercase();
    for marker in ["/api/webhooks/", "/services/"] {
        if let Some(idx) = low.find(marker) {
            let cut = idx + marker.len();
            if let Some(id_slash) = s[cut..].find('/') {
                let prefix_len = cut + id_slash + 1;
                return format!("{}{mask}", &s[..prefix_len]);
            }
        }
    }
    // 3. Streaming ingest URLs carry the stream key as trailing path segment.
    let is_ingest =
        ["rtmp://", "rtmps://", "srt://", "rist://", "whip://"].iter().any(|sch| scheme_prefix.ends_with(sch));
    if is_ingest {
        let path_part_end = s.find('?').unwrap_or(s.len());
        if let Some(slash) = s[..path_part_end].rfind('/')
            && slash > 0
            && slash + 1 < path_part_end
        {
            s = format!("{}{mask}{}", &s[..slash + 1], &s[path_part_end..]);
        }
    }
    s
}

fn scrub_kv_and_json_secrets(text: &str, mask: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let low = rest.to_ascii_lowercase();
        if let Some((key_len, val_delim)) = match_kv_prefix(&low) {
            let after_eq = &rest[key_len..];
            // Strip optional whitespace after `=` or `:` before quoted value.
            let trim_ws = after_eq.len() - after_eq.trim_start_matches([' ', '\t']).len();
            let v_start = &after_eq[trim_ws..];
            if let Some(q) = v_start.chars().next().filter(|&c| c == '"' || c == '\'') {
                let inner = &v_start[1..];
                let end = inner.find(q).unwrap_or(inner.len());
                out.push_str(&rest[..key_len + trim_ws + 1]);
                if inner[..end] == *mask || inner[..end] == *"•••" {
                    out.push_str(&inner[..end]);
                } else {
                    out.push_str(mask);
                }
                if end < inner.len() {
                    out.push(q);
                    rest = &inner[end + 1..];
                } else {
                    rest = "";
                }
                continue;
            }
            let end = after_eq
                .find(|c: char| c.is_whitespace() || matches!(c, '&' | ',' | ';' | '"' | '\'' | ')' | '}' | ']' | '|'))
                .unwrap_or(after_eq.len());
            out.push_str(&rest[..key_len]);
            let val = &after_eq[..end];
            if !val.is_empty() && !matches!(val, "null" | "true" | "false" | "none") && val != mask && val != "•••"
            {
                out.push_str(mask);
            } else if !val.is_empty() {
                out.push_str(val);
            }
            let _ = val_delim;
            rest = &after_eq[end..];
            continue;
        }
        let ch = rest.chars().next().unwrap_or('\0');
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

fn match_kv_prefix(lower: &str) -> Option<(usize, char)> {
    const KEYS: &[&str] = &[
        "client_secret",
        "access_token",
        "refresh_token",
        "authorization",
        "obsidian_key",
        "obs_password",
        "passphrase",
        "stream_key",
        "streamkey",
        "stream_id",
        "streamid",
        "id_token",
        "webhook_url",
        "webhook",
        "password",
        "api_key",
        "apikey",
        "passwd",
        "secret",
        "bearer",
        "token",
        "pass",
        "auth",
        "key",
    ];
    for k in KEYS {
        // Quoted JSON key: `"stream_key":`
        if let Some(after_q) = lower.strip_prefix('"')
            && let Some(after_k) = after_q.strip_prefix(k)
            && let Some(after_q2) = after_k.strip_prefix('"')
        {
            let ws = after_q2.len() - after_q2.trim_start_matches([' ', '\t']).len();
            if after_q2[ws..].starts_with(':') {
                let total = 1 + k.len() + 1 + ws + 1;
                let ws2 = lower[total..].len() - lower[total..].trim_start_matches([' ', '\t']).len();
                return Some((total + ws2, ':'));
            }
        }
        // Unquoted `key=` / `key:` (check boundary: word shouldn't be preceded? handled by scanner advancing on match; avoid matching suffix of unrelated word by checking if remaining starts cleanly - note the caller splits inside non-word loops when advancing char by char, Wait: let's guard non-identifier start via helper below if needed; here KEYS match cleanly).
        if let Some(after_k) = lower.strip_prefix(k)
            && let Some(d) = after_k.chars().next().filter(|&c| c == '=' || c == ':')
        {
            return Some((k.len() + 1, d));
        }
    }
    None
}

fn scrub_token_words(text: &str, mask: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut expect_auth_token = false;
    while !rest.is_empty() {
        let len = rest
            .find(|c: char| {
                c.is_whitespace()
                    || matches!(c, '"' | '\'' | '&' | '=' | ',' | ';' | '(' | ')' | '<' | '>' | '[' | ']' | '{' | '}')
            })
            .unwrap_or(rest.len());
        if len == 0 {
            let ch = rest.chars().next().unwrap_or('\0');
            if !ch.is_whitespace() {
                expect_auth_token = false;
            }
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
            continue;
        }
        let word = &rest[..len];
        let low = word.to_ascii_lowercase();
        if expect_auth_token {
            out.push_str(mask);
            expect_auth_token = false;
        } else if low == "bearer" || low == "basic" {
            out.push_str(word);
            expect_auth_token = true;
        } else if is_prefixed_token(word, &low) || is_jwt(word) || looks_opaque_secret(word) {
            out.push_str(mask);
        } else {
            out.push_str(word);
        }
        rest = &rest[len..];
    }
    out
}

fn is_prefixed_token(word: &str, lower: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "live_", "oauth:", "sk_live_", "sk_test_", "whsec_", "ghp_", "gho_", "ghu_", "aiza", "xoxb-", "xoxp-", "fcr_",
    ];
    PREFIXES.iter().any(|p| lower.starts_with(p) && word.len() > p.len() + 2)
}

fn is_jwt(word: &str) -> bool {
    if !word.starts_with("eyJ") {
        return false;
    }
    let mut parts = word.split('.');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(a), Some(b), Some(c), None) => !a.is_empty() && !b.is_empty() && !c.is_empty(),
        _ => false,
    }
}

fn looks_opaque_secret(w: &str) -> bool {
    if w.len() < 24 || w.contains('/') || w.contains('\\') || w.contains(':') {
        return false;
    }
    // Preserve pure hex / UUIDs unless long high-entropy mixed tokens.
    let valid = w.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
    if !valid {
        return false;
    }
    let upper = w.bytes().any(|b| b.is_ascii_uppercase());
    let lower = w.bytes().any(|b| b.is_ascii_lowercase());
    let digit = w.bytes().any(|b| b.is_ascii_digit());
    let sep = w.contains('_');
    (digit && lower && (upper || sep)) || (upper && lower && sep && w.len() >= 28)
}

fn scrub_emails(text: &str, mask: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut last_copied = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            let mut local_start = i;
            while local_start > last_copied && is_email_local(bytes[local_start - 1]) {
                local_start -= 1;
            }
            // Strip leading punctuation like `.`
            while local_start < i && bytes[local_start] == b'.' {
                local_start += 1;
            }
            let mut dom_end = i + 1;
            while dom_end < bytes.len() && is_email_domain(bytes[dom_end]) {
                dom_end += 1;
            }
            while dom_end > i + 1 && matches!(bytes[dom_end - 1], b'.' | b'-') {
                dom_end -= 1;
            }
            let local = &text[local_start..i];
            let dom = &text[i + 1..dom_end];
            if is_valid_email_parts(local, dom) {
                out.push_str(&text[last_copied..local_start]);
                out.push_str(mask);
                last_copied = dom_end;
                i = dom_end;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&text[last_copied..]);
    out
}

fn is_email_local(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'%' | b'+' | b'-')
}

fn is_email_domain(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-')
}

fn is_valid_email_parts(local: &str, dom: &str) -> bool {
    if local.is_empty() || dom.len() < 4 || !dom.contains('.') {
        return false;
    }
    let Some((_, tld)) = dom.rsplit_once('.') else { return false };
    tld.len() >= 2 && tld.bytes().all(|b| b.is_ascii_alphabetic()) && !dom.contains("..")
}

fn scrub_ip_addresses(text: &str, mask: &str) -> String {
    let spans = find_ip_spans(text);
    if spans.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end, bracketed) in spans {
        if start < cursor {
            continue;
        }
        out.push_str(&text[cursor..start]);
        if bracketed {
            out.push('[');
            out.push_str(mask);
            out.push(']');
        } else {
            out.push_str(mask);
        }
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// Finds `(start_byte, end_byte, is_bracketed)` of every valid IPv4/IPv6 literal (and optional
/// `%zone` suffix). Note `[ipv6]:port` scrubs the `[ipv6]` bracket interior and keeps `:port`.
fn find_ip_spans(text: &str) -> Vec<(usize, usize, bool)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Bracketed IPv6: `[...]` (may include `%zone`).
        if bytes[i] == b'[' {
            if let Some(close) = text[i + 1..].find(']') {
                let inner = &text[i + 1..i + 1 + close];
                let core = inner.split('%').next().unwrap_or(inner);
                if IpAddr::from_str(core).is_ok() {
                    out.push((i, i + 1 + close + 1, true));
                    i = i + 1 + close + 1;
                    continue;
                }
            }
            i += 1;
            continue;
        }

        if is_ip_char(bytes[i]) {
            let left_ok = i == 0 || !is_ip_boundary_blocker(bytes[i - 1]);
            let start = i;
            while i < bytes.len() && is_ip_char(bytes[i]) {
                i += 1;
            }
            // Optional IPv6 `%zone_id`
            let mut end = i;
            if end < bytes.len() && bytes[end] == b'%' {
                let z_start = end + 1;
                let mut z_end = z_start;
                while z_end < bytes.len()
                    && (bytes[z_end].is_ascii_alphanumeric() || matches!(bytes[z_end], b'_' | b'.' | b'-'))
                {
                    z_end += 1;
                }
                if z_end > z_start {
                    end = z_end;
                    i = z_end;
                }
            }
            if !left_ok {
                continue;
            }
            let right_ok = end == bytes.len() || !is_ip_boundary_blocker(bytes[end]);
            if !right_ok {
                continue;
            }

            let candidate = &text[start..end];
            if let Some((ip_end_rel, _)) = parse_ip_or_ip_port(candidate) {
                out.push((start, start + ip_end_rel, false));
            }
            continue;
        }
        i += 1;
    }
    out
}

fn is_ip_char(b: u8) -> bool {
    b.is_ascii_hexdigit() || matches!(b, b'.' | b':')
}

fn is_ip_boundary_blocker(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn parse_ip_or_ip_port(candidate: &str) -> Option<(usize, IpAddr)> {
    // Trim trailing `.` (sentence punctuation), unless it is part of nothing else, and handle optional `%zone`.
    let trimmed = candidate.trim_end_matches('.');
    let without_zone = trimmed.split('%').next().unwrap_or(trimmed);
    if let Ok(ip) = IpAddr::from_str(without_zone) {
        return Some((trimmed.len(), ip));
    }
    // Check `ipv4:port`
    if let Some((host, port)) = without_zone.rsplit_once(':')
        && !port.is_empty()
        && port.bytes().all(|b| b.is_ascii_digit())
        && let Ok(IpAddr::V4(v4)) = IpAddr::from_str(host)
    {
        return Some((host.len(), IpAddr::V4(v4)));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy_input() -> DiagnosticInput {
        DiagnosticInput {
            generated_at: Millis(1_700_000_000_000),
            app: AppVersionInfo {
                version: "1.4.2".into(),
                os: "macos".into(),
                arch: "aarch64".into(),
                build_mode: "release".into(),
            },
            os: OsVersionInfo {
                name: "macOS".into(),
                version: "15.1".into(),
                long_version: "macOS 15.1 Sequoia".into(),
                kernel_version: "24.1.0".into(),
            },
            gpu: GpuInfo {
                name: "Apple M3 Max".into(),
                backend: "Metal".into(),
                device_type: "IntegratedGpu".into(),
                driver: "".into(),
                driver_info: "".into(),
                is_software: false,
                active_renderer: Some("ferrocore".into()),
            },
            encoders: vec![
                EncoderInfo {
                    id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
                    label: "Apple VT H264 Hardware Encoder".into(),
                    hardware: true,
                    codec: "H.264".into(),
                    selected: true,
                },
                EncoderInfo {
                    id: "obs_x264".into(),
                    label: "x264".into(),
                    hardware: false,
                    codec: "H.264".into(),
                    selected: false,
                },
            ],
            configured_encoder_id: Some("com.apple.videotoolbox.videoencoder.ave.avc".into()),
            audio: AudioSummary {
                studio_sample_rate_hz: 48_000,
                monitoring_device: Some("Studio Headphones".into()),
                devices: vec![
                    AudioDeviceInfo {
                        kind: "input".into(),
                        name: "USB Mic".into(),
                        is_default: true,
                        sample_rate_hz: Some(48_000),
                        supported_sample_rates_hz: vec![44_100, 48_000],
                        channels: Some(2),
                    },
                    AudioDeviceInfo {
                        kind: "output".into(),
                        name: "Studio Headphones".into(),
                        is_default: true,
                        sample_rate_hz: Some(48_000),
                        supported_sample_rates_hz: vec![48_000],
                        channels: Some(2),
                    },
                ],
            },
            disk_free_mb: Some(120_000),
            ..Default::default()
        }
    }

    #[test]
    fn redaction_table_scrubs_every_secret_class_and_keeps_safe_metadata() {
        let fixture_secrets = ["mySuperSecretKey_9988", "twitch_custom_oauth_xyz123"];
        let fixture_users = ["arshramgarhia", "AliceWin"];

        let cases: &[(&str, &[&str])] = &[
            // Stream keys & URLs
            ("rtmp://live.twitch.tv/app/live_123456789_abcdef987654321", &["live_123456789_abcdef987654321"]),
            (
                "rtmps://a.rtmp.youtube.com/live2/xxxx-yyyy-zzzz-1111?secret=topSecretVal",
                &["xxxx-yyyy-zzzz-1111", "topSecretVal"],
            ),
            (
                "srt://ingest.example.com:9000?streamid=#!::r=live/abc,u=key&passphrase=mySrtPassword1",
                &["#!::r=live/abc,u=key", "mySrtPassword1"],
            ),
            ("https://ingestuser:verySecretPass@whip.example.com/whip/live", &["ingestuser", "verySecretPass"]),
            (
                "https://discord.com/api/webhooks/123456789012345678/abcdefghijklmnopqrstuvwxyz0123456789",
                &["abcdefghijklmnopqrstuvwxyz0123456789"],
            ),
            // Tokens & passwords (KV + JSON + Bearer + JWT)
            (
                r#"{"stream_key":"sk_secret_123","access_token":"tok_abc_999","password":"hunter2!","client_secret":"cs_999"}"#,
                &["sk_secret_123", "tok_abc_999", "hunter2!", "cs_999"],
            ),
            (
                "Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0In0.sigValueHere",
                &["eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9", "sigValueHere"],
            ),
            (
                "oauth:987654321secret0 oauth tok ghp_1234567890abcdefghijklmnop fcr_dv_abc123.xyz",
                &["oauth:987654321secret0", "ghp_1234567890abcdefghijklmnop", "fcr_dv_abc123.xyz"],
            ),
            // Emails
            (
                "account email=streamer.pro+live@clearcraft.io owner=bob@example.co.uk",
                &["streamer.pro+live@clearcraft.io", "bob@example.co.uk"],
            ),
            // IPv4 and IPv6
            (
                "listening on 127.0.0.1:47820 lan=192.168.1.42 remote=203.0.113.88 v6=[fe80::1ff:fe23:4567:890a]:47820 alt=2001:db8::1%en0",
                &["127.0.0.1", "192.168.1.42", "203.0.113.88", "fe80::1ff:fe23:4567:890a", "2001:db8::1"],
            ),
            // Usernames in paths (macOS, Linux, Windows, explicit username)
            (
                r"recording saved at /Users/arshramgarhia/Movies/live.mp4 home=/home/streamer99/data C:\Users\AliceWin\Videos\clip.mp4 mic=AliceWin's USB",
                &["arshramgarhia", "streamer99", "AliceWin"],
            ),
            // Fixture literal secrets
            (
                "custom=mySuperSecretKey_9988 another=twitch_custom_oauth_xyz123",
                &["mySuperSecretKey_9988", "twitch_custom_oauth_xyz123"],
            ),
        ];

        for (raw, forbidden) in cases {
            let red = redact_text(raw, &fixture_secrets, &fixture_users);
            for secret in *forbidden {
                assert!(!red.contains(secret), "secret {secret:?} survived in {red:?} from {raw:?}");
            }
        }

        // Preserve non-secret timestamps, SemVer versions, and normal log text
        let safe = "2026-11-22T03:02:11Z INFO ferrocast 1.4.2 Apple M3 Max 48000 Hz";
        assert_eq!(redact_text(safe, &fixture_secrets, &fixture_users), safe);
    }

    #[test]
    fn binary_dump_redaction_erases_ascii_and_utf16le_secrets() {
        let secrets = ["live_fixture_secret_98765", "myDumpPassword!"];
        let users = ["DumpUser"];
        let mut blob = Vec::new();
        blob.extend_from_slice(b"MDMP\x93\xa7\x00\x00");
        blob.extend_from_slice(b"PATH=/Users/DumpUser/Library/Ferrocast IP=198.51.100.44 email=test@dump.example stream_key=live_fixture_secret_98765 ");
        let utf16_chunk =
            r"C:\Users\DumpUser\Videos password=myDumpPassword! bearer=u16_generic_bearer_99 [2001:db8::42]";
        for u in utf16_chunk.encode_utf16() {
            blob.extend_from_slice(&u.to_le_bytes());
        }
        let orig_len = blob.len();

        redact_bytes_in_place(&mut blob, &secrets, &users);
        assert_eq!(blob.len(), orig_len, "binary RVA layout preserved");
        assert!(blob.starts_with(b"MDMP"));

        let as_ascii = String::from_utf8_lossy(&blob);
        for forbidden in ["DumpUser", "198.51.100.44", "test@dump.example", "live_fixture_secret_98765"] {
            assert!(!as_ascii.contains(forbidden), "ASCII secret {forbidden} survived");
        }
        for u16_forbidden in ["DumpUser", "myDumpPassword!", "u16_generic_bearer_99", "2001:db8::42"] {
            let pat: Vec<u8> = u16_forbidden.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
            assert!(!blob.windows(pat.len()).any(|w| w == pat.as_slice()), "UTF-16LE secret {u16_forbidden} survived");
        }
    }

    #[test]
    fn likely_cause_rules_cover_every_condition_and_healthy_baseline() {
        assert!(analyze_likely_causes(&healthy_input()).is_empty());

        // Crashes + unclean exit
        let mut crash = healthy_input();
        crash.crashes.push(CrashDumpSummary {
            filename: "ferrocast 1.4.2 2026-11-22 02-00-00.dmp".into(),
            modified_ms: Some(100),
            size_bytes: 1024,
            kind: "minidump".into(),
            included_in_zip: true,
            summary: None,
        });
        crash.unclean_exit = true;
        let c_causes = analyze_likely_causes(&crash);
        assert_eq!(c_causes[0].code, "crash_dumps_present");
        assert_eq!(c_causes[0].severity, CauseSeverity::Critical);
        assert!(c_causes.iter().any(|c| c.code == "unclean_exit_safe_mode"));

        // Software GPU + low disk + connector + log errors
        let mut sys_fail = healthy_input();
        sys_fail.gpu.is_software = true;
        sys_fail.gpu.device_type = "Cpu".into();
        sys_fail.disk_free_mb = Some(300);
        sys_fail.connectors.push(ConnectorDiagnostic {
            platform: Platform::Twitch,
            state: "sign_in_needed".into(),
            detail: Some("token expired".into()),
        });
        sys_fail.recent_log_errors.push("ERROR gpu: OutOfMemory".into());
        let codes: Vec<_> = analyze_likely_causes(&sys_fail).into_iter().map(|c| c.code).collect();
        assert!(codes.contains(&"gpu_software_fallback".into()));
        assert!(codes.contains(&"disk_space_low".into()));
        assert!(codes.contains(&"connector_auth_issue".into()));
        assert!(codes.contains(&"recent_log_errors".into()));

        // Stream health: dropped, skipped, lagged, slow frame_ms + audio sample rate mismatch + software encoder
        let mut stream_issues = healthy_input();
        stream_issues.configured_encoder_id = Some("obs_x264".into());
        stream_issues.audio.devices[0].sample_rate_hz = Some(44_100);
        stream_issues.last_streams.push(StreamHealthRecord {
            id: "s1".into(),
            started_at: Millis(1_000),
            ended_at: Some(Millis(10_000)),
            duration_s: 900,
            platforms: vec![Platform::Twitch],
            live: false,
            unclean_end: false,
            fps_target: 60,
            canvas: "1920x1080".into(),
            encoder_id: Some("obs_x264".into()),
            sample_rate_hz: 48_000,
            dropped_frames: 300,
            skipped_frames: 250,
            lagged_frames: 180,
            rendered_frames: 3000,
            avg_fps: 48.0,
            cpu_pct: 92.0,
            memory_mb: 1800,
            frame_ms: 25.0,
            destinations: vec![DestinationHealthSnapshot {
                platform: "twitch".into(),
                target: "rtmp://live.twitch.tv/app/live_secret999".into(),
                state: "live".into(),
                kbps: 6000,
                dropped_frames: 300,
                total_frames: 3000,
                rtt_ms: Some(120),
                lost_pct: Some(5.0),
                reconnects: 2,
                last_error: None,
            }],
            issues: vec!["drop".into()],
        });
        let report = build_support_report(stream_issues, &[], &["arshramgarhia"]);
        let codes: Vec<_> = report.likely_causes.iter().map(|c| c.code.as_str()).collect();
        for expected in [
            "stream_network_drops",
            "stream_encoder_overload",
            "stream_render_lag",
            "software_encoder_selected",
            "audio_sample_rate_mismatch",
        ] {
            assert!(codes.contains(&expected), "missing expected cause {expected} in {codes:?}");
        }
        let summary = format_summary_txt(&report, &[], &[]);
        assert!(summary.contains("=== LIKELY CAUSES ==="));
        assert!(!summary.contains("live_secret999"));
        let cause_pos = summary.find("=== LIKELY CAUSES ===").unwrap_or(usize::MAX);
        let app_pos = summary.find("=== APP & OS VERSIONS ===").unwrap_or(0);
        assert!(cause_pos < app_pos, "likely causes must appear at the top");
    }
}
