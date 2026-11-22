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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioSummary {
    pub studio_sample_rate_hz: u32,
    pub monitoring_device: Option<String>,
    pub devices: Vec<AudioDeviceInfo>,
    #[serde(default = "yes_true")]
    pub enumeration_supported: bool,
}

impl Default for AudioSummary {
    fn default() -> Self {
        Self { studio_sample_rate_hz: 0, monitoring_device: None, devices: Vec::new(), enumeration_supported: true }
    }
}

fn yes_true() -> bool {
    true
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

/// A destination turned on or off while the stream ran (or at its start).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DestinationEvent {
    pub id: String,
    pub name: String,
    pub at: Millis,
    /// `true` = it started sending, `false` = it stopped.
    pub on: bool,
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
    /// Which destinations were on, and when they were turned on or off (older records: none).
    #[serde(default)]
    pub destination_log: Vec<DestinationEvent>,
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
    report.gpu.backend = sc(&report.gpu.backend);
    report.gpu.device_type = sc(&report.gpu.device_type);
    report.gpu.driver = sc(&report.gpu.driver);
    report.gpu.driver_info = sc(&report.gpu.driver_info);
    report.gpu.active_renderer = report.gpu.active_renderer.as_deref().map(sc);
    for enc in &mut report.encoders {
        enc.id = sc(&enc.id);
        enc.label = sc(&enc.label);
        enc.codec = sc(&enc.codec);
    }
    if let Some(m) = &mut report.audio.monitoring_device {
        *m = sc(m);
    }
    for d in &mut report.audio.devices {
        d.kind = sc(&d.kind);
        d.name = sc(&d.name);
    }
    for st in &mut report.last_streams {
        st.id = sc(&st.id);
        st.canvas = sc(&st.canvas);
        st.encoder_id = st.encoder_id.as_deref().map(sc);
        for d in &mut st.destinations {
            d.platform = sc(&d.platform);
            d.target = sc(&d.target);
            d.state = sc(&d.state);
            d.last_error = d.last_error.as_deref().map(sc);
        }
        for issue in &mut st.issues {
            *issue = sc(issue);
        }
    }
    for cr in &mut report.crashes {
        cr.filename = sc(&cr.filename);
        cr.kind = sc(&cr.kind);
        cr.summary = cr.summary.as_deref().map(sc);
    }
    for cn in &mut report.connectors {
        cn.state = sc(&cn.state);
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

    if input.audio.enumeration_supported && !input.audio.devices.iter().any(|d| d.kind == "input") {
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
    if !report.audio.enumeration_supported {
        let _ = writeln!(s, "Audio devices: enumeration not supported on this OS");
    } else if report.audio.devices.is_empty() {
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
    let spans = collect_sensitive_spans(text, known_secrets, known_usernames);
    if spans.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for sp in spans {
        if sp.start < cursor || sp.end > text.len() {
            continue;
        }
        out.push_str(&text[cursor..sp.start]);
        let placeholder = match (style, sp.kind) {
            (RedactStyle::Report, SpanKind::Secret) => REDACTED,
            (RedactStyle::Report, SpanKind::Email) => REDACTED_EMAIL,
            (RedactStyle::Report, SpanKind::Ip) => REDACTED_IP,
            (RedactStyle::Report, SpanKind::User) => REDACTED_USER,
            (RedactStyle::Bullets, _) => "•••",
        };
        if sp.bracketed {
            out.push('[');
            out.push_str(placeholder);
            out.push(']');
        } else {
            out.push_str(placeholder);
        }
        cursor = sp.end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// Scrubs both UTF-8/ASCII and UTF-16LE representations of secrets, emails, IPs, and path
/// usernames in-place inside a raw binary buffer (such as a Windows `.dmp` minidump), overwriting
/// matched sensitive bytes with `*` (`0x2a` ASCII or `0x2a 0x00` UTF-16LE) to preserve fixed
/// offsets and RVA layout.
pub fn redact_bytes_in_place(buf: &mut [u8], known_secrets: &[&str], known_usernames: &[&str]) {
    if buf.is_empty() {
        return;
    }

    for s in known_secrets.iter().copied().filter(|s| s.len() >= 4) {
        scrub_ascii_needle(buf, s.as_bytes());
        scrub_utf16le_needle(buf, s);
    }
    for u in known_usernames.iter().copied().filter(|u| u.len() >= 3) {
        scrub_ascii_needle(buf, u.as_bytes());
        scrub_utf16le_needle(buf, u);
    }

    redact_utf8_runs_in_place(buf, known_secrets, known_usernames);
    redact_utf16le_runs_in_place(buf, known_secrets, known_usernames);
}

fn scrub_ascii_needle(data: &mut [u8], needle: &[u8]) {
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
}

fn scrub_utf16le_needle(data: &mut [u8], s: &str) {
    let units: Vec<u16> = s.encode_utf16().collect();
    let blen = units.len() * 2;
    if blen == 0 || blen > data.len() {
        return;
    }
    let pattern: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
    let lower_pattern: Vec<u8> = s.to_lowercase().encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let upper_pattern: Vec<u8> = s.to_uppercase().encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let mut i = 0;
    while i + blen <= data.len() {
        let slice = &data[i..i + blen];
        if slice == pattern.as_slice()
            || (lower_pattern.len() == blen && slice.eq_ignore_ascii_case_utf16(&lower_pattern))
            || (upper_pattern.len() == blen && slice.eq_ignore_ascii_case_utf16(&upper_pattern))
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
}

fn redact_utf8_runs_in_place(buf: &mut [u8], known_secrets: &[&str], known_usernames: &[&str]) {
    let mut i = 0;
    while i < buf.len() {
        if let Some(first_len) = next_utf8_text_char_len(buf, i) {
            let start = i;
            i += first_len;
            while i < buf.len() {
                if let Some(len) = next_utf8_text_char_len(buf, i) {
                    i += len;
                } else {
                    break;
                }
            }
            if i - start >= 6
                && let Ok(text) = std::str::from_utf8(&buf[start..i])
            {
                let spans = collect_sensitive_spans(text, known_secrets, known_usernames);
                for sp in spans {
                    for p in (start + sp.start)..(start + sp.end).min(buf.len()) {
                        if !matches!(buf[p], b'\r' | b'\n') {
                            buf[p] = b'*';
                        }
                    }
                }
            }
        } else {
            i += 1;
        }
    }
}

fn next_utf8_text_char_len(buf: &[u8], i: usize) -> Option<usize> {
    let b = buf[i];
    if b < 0x80 {
        return ((0x20..=0x7e).contains(&b) || matches!(b, b'\t' | b'\r' | b'\n')).then_some(1);
    }
    let expected = match b {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    let slice = buf.get(i..i + expected)?;
    let s = std::str::from_utf8(slice).ok()?;
    let ch = s.chars().next()?;
    if ch.len_utf8() != expected || ch.is_control() {
        return None;
    }
    Some(expected)
}

fn redact_utf16le_runs_in_place(buf: &mut [u8], known_secrets: &[&str], known_usernames: &[&str]) {
    for offset in [0usize, 1usize] {
        let mut idx = offset;
        while idx + 1 < buf.len() {
            let Some((first_ch, first_len, first_lat1)) = decode_utf16le_text_char(buf, idx) else {
                idx += 2;
                continue;
            };
            if !first_lat1 {
                idx += first_len;
                continue;
            }
            let mut text = String::new();
            let mut raw_ranges: Vec<(usize, usize)> = Vec::new();
            push_utf16_mapped_char(&mut text, &mut raw_ranges, first_ch, idx, idx + first_len);
            idx += first_len;
            let mut non_lat1_streak = 0usize;

            while idx + 1 < buf.len() {
                let Some((ch, clen, is_lat1)) = decode_utf16le_text_char(buf, idx) else {
                    break;
                };
                if is_lat1 {
                    non_lat1_streak = 0;
                } else {
                    if non_lat1_streak >= 64 {
                        break;
                    }
                    non_lat1_streak += 1;
                }
                push_utf16_mapped_char(&mut text, &mut raw_ranges, ch, idx, idx + clen);
                idx += clen;
            }

            if text.len() >= 6 {
                let spans = collect_sensitive_spans(&text, known_secrets, known_usernames);
                for sp in spans {
                    let Some(&(raw_start, _)) = raw_ranges.get(sp.start) else {
                        continue;
                    };
                    let Some(&(_, raw_end)) = raw_ranges.get(sp.end - 1) else {
                        continue;
                    };
                    for p in (raw_start..raw_end).step_by(2) {
                        if p + 1 < buf.len() && !(buf[p + 1] == 0 && matches!(buf[p], b'\r' | b'\n')) {
                            buf[p] = b'*';
                            buf[p + 1] = 0;
                        }
                    }
                }
            }
            idx += 2;
        }
    }
}

fn decode_utf16le_text_char(buf: &[u8], idx: usize) -> Option<(char, usize, bool)> {
    let lo = buf[idx];
    let hi = buf[idx + 1];
    let u = u16::from_le_bytes([lo, hi]);
    if u == 0 {
        return None;
    }
    if hi == 0 && (matches!(lo, b'\t' | b'\r' | b'\n') || (0x20..=0x7e).contains(&lo) || lo >= 0xa0) {
        return Some((char::from(lo), 2, true));
    }
    if (0xd800..=0xdbff).contains(&u) {
        let u2 = u16::from_le_bytes([*buf.get(idx + 2)?, *buf.get(idx + 3)?]);
        if !(0xdc00..=0xdfff).contains(&u2) {
            return None;
        }
        let ch = char::decode_utf16([u, u2]).next()?.ok()?;
        if ch.is_control() || ('\u{f0000}'..='\u{10fffd}').contains(&ch) {
            return None;
        }
        return Some((ch, 4, false));
    }
    if (0xdc00..=0xdfff).contains(&u) {
        return None;
    }
    let ch = char::from_u32(u as u32)?;
    if ch.is_control() || ('\u{e000}'..='\u{f8ff}').contains(&ch) {
        return None;
    }
    Some((ch, 2, false))
}

fn push_utf16_mapped_char(
    text: &mut String,
    raw_ranges: &mut Vec<(usize, usize)>,
    ch: char,
    raw_start: usize,
    raw_end: usize,
) {
    text.push(ch);
    for _ in 0..ch.len_utf8() {
        raw_ranges.push((raw_start, raw_end));
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SpanKind {
    Secret,
    Email,
    Ip,
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SensitiveSpan {
    start: usize,
    end: usize,
    kind: SpanKind,
    bracketed: bool,
}

fn collect_sensitive_spans(text: &str, known_secrets: &[&str], known_usernames: &[&str]) -> Vec<SensitiveSpan> {
    let mut spans = Vec::new();

    collect_known_spans(text, known_secrets, known_usernames, &mut spans);

    // 3. Structural collectors on raw text.
    collect_structural_spans(text, &mut spans);

    // 4. Percent-decoded pass for URL-encoded secrets/URLs/emails/paths (mapped back to raw spans).
    if text.contains('%') {
        collect_percent_decoded_spans(text, known_secrets, known_usernames, &mut spans);
    }

    merge_sensitive_spans(spans)
}

/// Registered secrets (>= 4 chars) and usernames (>= 3, on word boundaries).
fn collect_known_spans(text: &str, known_secrets: &[&str], known_usernames: &[&str], spans: &mut Vec<SensitiveSpan>) {
    let mut secrets: Vec<&str> = known_secrets.iter().copied().filter(|s| s.len() >= 4).collect();
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    for secret in secrets {
        for (idx, _) in text.match_indices(secret) {
            spans.push(SensitiveSpan { start: idx, end: idx + secret.len(), kind: SpanKind::Secret, bracketed: false });
        }
    }
    let mut users: Vec<&str> = known_usernames.iter().copied().filter(|u| u.len() >= 3).collect();
    users.sort_by_key(|u| std::cmp::Reverse(u.len()));
    for user in users {
        collect_identifier_ci_spans(text, user, spans);
    }
}

fn collect_structural_spans(text: &str, spans: &mut Vec<SensitiveSpan>) {
    collect_path_username_spans(text, spans);
    collect_pem_spans(text, spans);
    collect_url_spans(text, spans);
    collect_kv_and_json_spans(text, spans);
    collect_token_word_spans(text, spans);
    collect_email_spans(text, spans);
    for (start, end, bracketed) in find_ip_spans(text) {
        spans.push(SensitiveSpan { start, end, kind: SpanKind::Ip, bracketed });
    }
}

fn merge_sensitive_spans(mut spans: Vec<SensitiveSpan>) -> Vec<SensitiveSpan> {
    spans.retain(|s| s.start < s.end);
    if spans.is_empty() {
        return spans;
    }
    spans.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| b.end.cmp(&a.end)).then_with(|| a.kind.cmp(&b.kind)));
    let mut merged: Vec<SensitiveSpan> = Vec::with_capacity(spans.len());
    for sp in spans {
        if let Some(last) = merged.last_mut()
            && sp.start < last.end
        {
            // Keep bracketed IP boundaries intact when absorbing an interior match.
            if sp.end <= last.end {
                if sp.start == last.start && sp.end == last.end && sp.kind < last.kind {
                    last.kind = sp.kind;
                }
            } else {
                last.end = sp.end;
                if sp.kind < last.kind {
                    last.kind = sp.kind;
                }
                last.bracketed = false;
            }
        } else {
            merged.push(sp);
        }
    }
    merged
}

fn collect_percent_decoded_spans(
    text: &str,
    known_secrets: &[&str],
    known_usernames: &[&str],
    spans: &mut Vec<SensitiveSpan>,
) {
    let bytes = text.as_bytes();
    let mut decoded = String::with_capacity(text.len());
    let mut orig_ranges: Vec<(usize, usize)> = Vec::with_capacity(text.len());
    let mut i = 0usize;
    let mut saw_pct = false;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(&h), Some(&l)) = (bytes.get(i + 1), bytes.get(i + 2))
            && h.is_ascii_hexdigit()
            && l.is_ascii_hexdigit()
            && let Ok(val) = u8::from_str_radix(&text[i + 1..i + 3], 16)
            && (0x20..=0x7e).contains(&val)
        {
            saw_pct = true;
            decoded.push(val as char);
            orig_ranges.push((i, i + 3));
            i += 3;
            continue;
        }
        let ch = text[i..].chars().next().unwrap_or('\0');
        let clen = ch.len_utf8();
        decoded.push(ch);
        for _ in 0..clen {
            orig_ranges.push((i, i + clen));
        }
        i += clen;
    }
    if !saw_pct {
        return;
    }
    // The decoded text gets everything the raw text does: a registered credential written
    // with %2d for its hyphens has no key label to give it away.
    let mut dec_spans = Vec::new();
    collect_known_spans(&decoded, known_secrets, known_usernames, &mut dec_spans);
    collect_structural_spans(&decoded, &mut dec_spans);
    for ds in dec_spans {
        if ds.start < ds.end && ds.end <= orig_ranges.len() {
            let orig_start = orig_ranges[ds.start].0;
            let orig_end = orig_ranges[ds.end - 1].1;
            if orig_start < orig_end {
                spans.push(SensitiveSpan { start: orig_start, end: orig_end, kind: ds.kind, bracketed: ds.bracketed });
            }
        }
    }
}

fn collect_identifier_ci_spans(text: &str, needle: &str, spans: &mut Vec<SensitiveSpan>) {
    let low_text = text.to_lowercase();
    let low_needle = needle.to_lowercase();
    if low_text.len() != text.len() || low_needle.len() != needle.len() || low_needle.is_empty() {
        for (idx, _) in text.match_indices(needle) {
            spans.push(SensitiveSpan { start: idx, end: idx + needle.len(), kind: SpanKind::User, bracketed: false });
        }
        return;
    }
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while let Some(rel) = low_text[i..].find(&low_needle) {
        let start = i + rel;
        let end = start + needle.len();
        let left_ok = start == 0 || !is_ident_char(bytes[start - 1]);
        let right_ok = end == bytes.len() || !is_ident_char(bytes[end]) || text[end..].starts_with("'s");
        if left_ok && right_ok {
            spans.push(SensitiveSpan { start, end, kind: SpanKind::User, bracketed: false });
        }
        i = end;
    }
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn collect_path_username_spans(text: &str, spans: &mut Vec<SensitiveSpan>) {
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let lbytes = lower.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let matched = ["/users/", "/home/", "\\users\\", "/users\\", "\\users/"]
            .into_iter()
            .find(|p| lbytes[i..].starts_with(p.as_bytes()));

        if let Some(pref) = matched {
            let prev_ok = i == 0
                || bytes[i - 1] == b':'
                || bytes[i - 1].is_ascii_whitespace()
                || matches!(
                    bytes[i - 1],
                    b'"' | b'\'' | b'(' | b'[' | b'{' | b'=' | b'<' | b'\\' | b'/' | b',' | b';' | b'|'
                );
            if prev_ok {
                let u_start = i + pref.len();
                let u_end = find_path_segment_end(text, u_start);
                if u_end > u_start {
                    let seg = &text[u_start..u_end];
                    if !matches!(seg, "Public" | "Shared" | "Default" | "All Users" | "Default User")
                        && seg != REDACTED_USER
                        && seg != "•••"
                    {
                        spans.push(SensitiveSpan {
                            start: u_start,
                            end: u_end,
                            kind: SpanKind::User,
                            bracketed: false,
                        });
                        i = u_end;
                        continue;
                    }
                }
            }
        }
        let ch = text[i..].chars().next().unwrap_or('\0');
        i += ch.len_utf8();
    }
}

fn find_path_segment_end(text: &str, start: usize) -> usize {
    let mut end = start;
    let mut spaces = 0usize;
    for (rel, ch) in text[start..].char_indices() {
        if ch == ' ' {
            // Allow spaces in OS folder usernames (e.g. `C:\Users\John Smith\AppData`) when bounded by a
            // subsequent separator with no trailing space immediately before the separator.
            if spaces < 2 && rel > 0 {
                let next_part = &text[start + rel + 1..];
                if let Some(sep_pos) = next_part.find(['/', '\\'])
                    && sep_pos > 0
                    && sep_pos <= 32
                {
                    let seg_tail = &next_part[..sep_pos];
                    let has_bad = seg_tail.chars().any(|c| {
                        matches!(
                            c,
                            '\t' | '\r'
                                | '\n'
                                | '"'
                                | '\''
                                | '`'
                                | ','
                                | ';'
                                | ')'
                                | ']'
                                | '}'
                                | '>'
                                | '?'
                                | '&'
                                | '='
                                | ':'
                        )
                    });
                    if !has_bad && !seg_tail.ends_with(' ') && !next_part.starts_with(' ') {
                        spaces += 1;
                        end = start + rel + 1;
                        continue;
                    }
                }
            }
            break;
        }
        if matches!(
            ch,
            '/' | '\\' | '\t' | '\r' | '\n' | '"' | '\'' | '`' | ',' | ';' | ')' | ']' | '}' | '>' | '?' | '&' | '='
        ) {
            break;
        }
        end = start + rel + ch.len_utf8();
    }
    end
}

fn collect_pem_spans(text: &str, spans: &mut Vec<SensitiveSpan>) {
    let upper = text.to_ascii_uppercase();
    let mut pos = 0usize;
    while let Some(rel) = upper[pos..].find("-----BEGIN ") {
        let begin_start = pos + rel;
        let Some(dash_end_rel) = upper[begin_start + 11..].find("-----") else {
            break;
        };
        let header_end = begin_start + 11 + dash_end_rel + 5;
        let header_label = &upper[begin_start + 11..begin_start + 11 + dash_end_rel];
        if !(header_label.contains("PRIVATE KEY") || header_label.contains("SECRET KEY")) {
            pos = header_end;
            continue;
        }
        let body_start = if text[header_end..].starts_with("\r\n") {
            header_end + 2
        } else if text[header_end..].starts_with('\n') {
            header_end + 1
        } else {
            header_end
        };
        if let Some(end_rel) = upper[body_start..].find("-----END ") {
            let body_end = body_start + end_rel;
            let trimmed_end = text[body_start..body_end].trim_end_matches(['\r', '\n']).len() + body_start;
            if trimmed_end > body_start {
                spans.push(SensitiveSpan {
                    start: body_start,
                    end: trimmed_end,
                    kind: SpanKind::Secret,
                    bracketed: false,
                });
            }
            let after_end = upper[body_end..].find("-----").map_or(text.len(), |n| body_end + n + 5);
            pos = after_end;
        } else {
            // Truncated PEM block at log tail: scrub PEM header/base64 lines until non-PEM text or EOF.
            let mut cur = body_start;
            let mut body_end = body_start;
            for line in text[body_start..].split_inclusive('\n') {
                let t = line.trim();
                let looks_pem = t.is_empty()
                    || t.starts_with("Proc-Type:")
                    || t.starts_with("DEK-Info:")
                    || t.starts_with("Version:")
                    || t.starts_with("Comment:")
                    || (t.len() >= 4
                        && t.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')));
                if !looks_pem {
                    break;
                }
                cur += line.len();
                body_end = cur;
            }
            let trimmed_end = text[body_start..body_end].trim_end_matches(['\r', '\n']).len() + body_start;
            if trimmed_end > body_start {
                spans.push(SensitiveSpan {
                    start: body_start,
                    end: trimmed_end,
                    kind: SpanKind::Secret,
                    bracketed: false,
                });
            }
            pos = body_end.max(body_start);
        }
    }
}

fn collect_url_spans(text: &str, spans: &mut Vec<SensitiveSpan>) {
    let mut i = 0usize;
    let lower = text.to_ascii_lowercase();
    while let Some(rel) = lower[i..].find("://") {
        let scheme_end = i + rel + 3;
        let mut scheme_start = i + rel;
        while scheme_start > i
            && (lower.as_bytes()[scheme_start - 1].is_ascii_alphanumeric()
                || matches!(lower.as_bytes()[scheme_start - 1], b'+' | b'-' | b'.'))
        {
            scheme_start -= 1;
        }
        let scheme_prefix = &lower[scheme_start..scheme_end];
        let rest = &text[scheme_end..];
        let url_len = rest
            .find(|c: char| c.is_ascii_whitespace() || matches!(c, '"' | '\'' | ')' | '>' | '<' | '`'))
            .unwrap_or(rest.len());
        let body = &rest[..url_len];

        // 1. Userinfo (`user:pass@host`) in any URL authority.
        let auth_end = body.find(['/', '?', '#']).unwrap_or(body.len());
        if let Some(at) = body[..auth_end].rfind('@')
            && at > 0
        {
            spans.push(SensitiveSpan {
                start: scheme_end,
                end: scheme_end + at,
                kind: SpanKind::Secret,
                bracketed: false,
            });
        }

        // 2. Discord / Slack webhooks.
        let low_body = body.to_ascii_lowercase();
        let mut webhook_matched = false;
        for marker in ["/api/webhooks/", "/services/"] {
            if let Some(idx) = low_body.find(marker) {
                let cut = idx + marker.len();
                if let Some(id_slash) = body[cut..].find('/') {
                    let prefix_len = cut + id_slash + 1;
                    if prefix_len < url_len {
                        spans.push(SensitiveSpan {
                            start: scheme_end + prefix_len,
                            end: scheme_end + url_len,
                            kind: SpanKind::Secret,
                            bracketed: false,
                        });
                    }
                    webhook_matched = true;
                    break;
                }
            }
        }

        // 3. Streaming ingest URLs (`rtmp*`, `srt*`, `rist`, `whip`) and HTTPS/HTTP WHIP/WebRTC publish endpoints.
        if !webhook_matched {
            let path_part_end = body.find(['?', '#']).unwrap_or(body.len());
            let trimmed_end = body[..path_part_end].trim_end_matches('/').len();
            let is_rtmp_or_stream =
                ["rtmp://", "rtmps://", "rtmpt://", "rtmpe://", "rtmpte://", "srt://", "srtla://", "rist://"]
                    .contains(&scheme_prefix);
            let is_whip = scheme_prefix == "whip://";
            if is_rtmp_or_stream || is_whip {
                if let Some(host_slash) = body[..trimmed_end].find('/') {
                    if let Some(key_slash_rel) = body[host_slash + 1..trimmed_end].find('/') {
                        let key_start = host_slash + 1 + key_slash_rel + 1;
                        if key_start < trimmed_end {
                            spans.push(SensitiveSpan {
                                start: scheme_end + key_start,
                                end: scheme_end + trimmed_end,
                                kind: SpanKind::Secret,
                                bracketed: false,
                            });
                        }
                    } else if is_whip && host_slash + 1 < trimmed_end {
                        spans.push(SensitiveSpan {
                            start: scheme_end + host_slash + 1,
                            end: scheme_end + trimmed_end,
                            kind: SpanKind::Secret,
                            bracketed: false,
                        });
                    }
                }
            } else if scheme_prefix == "https://" || scheme_prefix == "http://" {
                let low_path = &low_body[..trimmed_end];
                for marker in ["/whip/", "/webrtc/publish/"] {
                    if let Some(m_idx) = low_path.find(marker) {
                        let key_start = m_idx + marker.len();
                        if key_start < trimmed_end {
                            spans.push(SensitiveSpan {
                                start: scheme_end + key_start,
                                end: scheme_end + trimmed_end,
                                kind: SpanKind::Secret,
                                bracketed: false,
                            });
                        }
                        break;
                    }
                }
            }
        }

        // `://` is consumed, so this always moves on, and never past the end.
        i = scheme_end + url_len;
    }
}

fn find_closing_quote(inner: &str, quote: char, escaped_json: bool) -> Option<usize> {
    let bytes = inner.as_bytes();
    let q_byte = quote as u8;
    let mut i = 0usize;
    if escaped_json {
        // Inside an escaped JSON string (`\"key\":\"val\"`), the closing delimiter is `\"` with an odd
        // number of backslashes where `% 4 == 1` (not `\\\"` which is an escaped inner quote).
        while i < bytes.len() {
            if bytes[i] == q_byte {
                let mut bs = 0usize;
                while bs < i && bytes[i - 1 - bs] == b'\\' {
                    bs += 1;
                }
                if bs % 4 == 1 {
                    return Some(i - 1);
                }
                if bs == 0 {
                    // Unescaped outer quote reached unexpectedly; stop before it.
                    return Some(i);
                }
            } else if bytes[i] == b'\n' || bytes[i] == b'\r' {
                return Some(i);
            }
            i += 1;
        }
        return None;
    }
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i = (i + 2).min(bytes.len());
            continue;
        }
        if bytes[i] == q_byte {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn collect_kv_and_json_spans(text: &str, spans: &mut Vec<SensitiveSpan>) {
    let mut offset = 0usize;
    // Lowercased once (ASCII only, so every offset is the same in both).
    let lower_all = text.to_ascii_lowercase();
    while offset < text.len() {
        let rest = &text[offset..];
        let prev_byte = offset.checked_sub(1).map(|idx| text.as_bytes()[idx]);
        let low = &lower_all[offset..];
        if let Some((key_name, key_len, val_delim, is_json_key, is_escaped_json)) = match_kv_prefix(low, prev_byte) {
            let after_eq = &rest[key_len..];
            let trim_ws = if is_json_key {
                after_eq.len() - after_eq.trim_start_matches([' ', '\t', '\r', '\n']).len()
            } else {
                after_eq.len() - after_eq.trim_start_matches([' ', '\t']).len()
            };
            let v_start = &after_eq[trim_ws..];

            if is_escaped_json && v_start.starts_with("\\\"") {
                let inner = &v_start[2..];
                let end = find_closing_quote(inner, '"', true).unwrap_or(inner.len());
                let val = &inner[..end];
                if !is_placeholder_or_benign(val) {
                    let abs_start = offset + key_len + trim_ws + 2;
                    spans.push(SensitiveSpan {
                        start: abs_start,
                        end: abs_start + end,
                        kind: SpanKind::Secret,
                        bracketed: false,
                    });
                }
                let advance = key_len + trim_ws + 2 + end + usize::from(inner[end..].starts_with("\\\"")) * 2;
                offset += advance.max(1);
                continue;
            }

            if let Some(q) = v_start.chars().next().filter(|&c| c == '"' || c == '\'') {
                let inner = &v_start[1..];
                let end = find_closing_quote(inner, q, false).unwrap_or(inner.len());
                let val = &inner[..end];
                if !is_placeholder_or_benign(val) {
                    let abs_start = offset + key_len + trim_ws + 1;
                    spans.push(SensitiveSpan {
                        start: abs_start,
                        end: abs_start + end,
                        kind: SpanKind::Secret,
                        bracketed: false,
                    });
                }
                let has_close = end < inner.len();
                offset += key_len + trim_ws + 1 + end + usize::from(has_close);
                continue;
            }

            // HTTP Cookie / Set-Cookie header lines hold semicolon-separated pairs up to EOL/quote.
            if matches!(key_name, "cookie" | "set-cookie") && val_delim == ':' {
                let end = v_start.find(['\r', '\n', '"', '\'']).unwrap_or(v_start.len());
                let val = v_start[..end].trim_end();
                if !val.is_empty() && !is_placeholder_or_benign(val) {
                    let abs_start = offset + key_len + trim_ws;
                    spans.push(SensitiveSpan {
                        start: abs_start,
                        end: abs_start + val.len(),
                        kind: SpanKind::Secret,
                        bracketed: false,
                    });
                }
                offset += (key_len + trim_ws + end).max(1);
                continue;
            }

            let mut end = v_start
                .find(|c: char| c.is_whitespace() || matches!(c, '&' | ',' | ';' | '"' | '\'' | ')' | '}' | ']' | '|'))
                .unwrap_or(v_start.len());
            let first = &v_start[..end];
            if key_name == "authorization" && val_delim == ':' {
                if first.eq_ignore_ascii_case("bearer") || first.eq_ignore_ascii_case("basic") {
                    offset += (key_len + trim_ws + end).max(1);
                    continue;
                }
                let rem = &v_start[end..];
                let ws2 = rem.len() - rem.trim_start_matches([' ', '\t']).len();
                if ws2 > 0 {
                    let after_ws2 = &rem[ws2..];
                    let end2 = after_ws2
                        .find(|c: char| {
                            c.is_whitespace() || matches!(c, '&' | ',' | ';' | '"' | '\'' | ')' | '}' | ']' | '|')
                        })
                        .unwrap_or(after_ws2.len());
                    if end2 > 0 {
                        end += ws2 + end2;
                    }
                }
            }
            let val = &v_start[..end];
            if !val.is_empty() && !is_placeholder_or_benign(val) {
                let abs_start = offset + key_len + trim_ws;
                spans.push(SensitiveSpan {
                    start: abs_start,
                    end: abs_start + end,
                    kind: SpanKind::Secret,
                    bracketed: false,
                });
            }
            offset += (key_len + trim_ws + end).max(1);
            continue;
        }
        let ch = rest.chars().next().unwrap_or('\0');
        offset += ch.len_utf8();
    }
}

fn is_placeholder_or_benign(val: &str) -> bool {
    matches!(val.to_ascii_lowercase().as_str(), "null" | "true" | "false" | "none")
        || matches!(val, REDACTED | REDACTED_USER | REDACTED_EMAIL | REDACTED_IP | "•••")
}

fn match_kv_prefix(lower: &str, prev_byte: Option<u8>) -> Option<(&'static str, usize, char, bool, bool)> {
    const KEYS: &[&str] = &[
        "x-amz-security-token",
        "x-amz-signature",
        "webhook_secret",
        "signing_secret",
        "install_secret",
        "client_secret",
        "code_verifier",
        "access_token",
        "refresh_token",
        "authorization",
        "x-auth-token",
        "x-csrf-token",
        "obsidian_key",
        "obs_password",
        "private_key",
        "webhook_url",
        "oauth_code",
        "session_id",
        "passphrase",
        "stream_key",
        "streamkey",
        "stream_id",
        "streamid",
        "id_token",
        "csrf_token",
        "set-cookie",
        "x-api-key",
        "auth_code",
        "signature",
        "password",
        "webhook",
        "api_key",
        "apikey",
        "session",
        "passwd",
        "secret",
        "bearer",
        "cookie",
        "ticket",
        "token",
        "pass",
        "auth",
        "pair",
        "code",
        "jwt",
        "pwd",
        "key",
        "sig",
    ];
    let prev_alnum = prev_byte.is_some_and(|b| b.is_ascii_alphanumeric());
    for k in KEYS {
        // Escaped JSON key: `\"stream_key\":`
        if let Some(after_q) = lower.strip_prefix("\\\"")
            && let Some(after_k) = after_q.strip_prefix(k)
            && let Some(after_q2) = after_k.strip_prefix("\\\"")
        {
            let ws = after_q2.len() - after_q2.trim_start_matches([' ', '\t', '\r', '\n']).len();
            if after_q2[ws..].starts_with(':') {
                let total = 2 + k.len() + 2 + ws + 1;
                let ws2 = lower[total..].len() - lower[total..].trim_start_matches([' ', '\t', '\r', '\n']).len();
                return Some((k, total + ws2, ':', true, true));
            }
        }
        // Quoted JSON key: `"stream_key":`
        if let Some(after_q) = lower.strip_prefix('"')
            && let Some(after_k) = after_q.strip_prefix(k)
            && let Some(after_q2) = after_k.strip_prefix('"')
        {
            // Keep `LikelyCause.code` intact in `report.json`; only redact `code` in URL queries.
            if *k == "code" {
                continue;
            }
            let ws = after_q2.len() - after_q2.trim_start_matches([' ', '\t', '\r', '\n']).len();
            if after_q2[ws..].starts_with(':') {
                let total = 1 + k.len() + 1 + ws + 1;
                let ws2 = lower[total..].len() - lower[total..].trim_start_matches([' ', '\t', '\r', '\n']).len();
                return Some((k, total + ws2, ':', true, false));
            }
        }
        // Unquoted `key=` / `key:` on a non-alphanumeric left boundary (`obs_password=` is fine; `hotkey=` is not).
        if !prev_alnum
            && let Some(after_k) = lower.strip_prefix(k)
            && let Some(d) = after_k.chars().next().filter(|&c| c == '=' || c == ':')
        {
            if *k == "code" && !(d == '=' && matches!(prev_byte, Some(b'?' | b'&'))) {
                continue;
            }
            if *k == "pair" && !(d == '=' && matches!(prev_byte, Some(b'?' | b'&' | b'#'))) {
                continue;
            }
            return Some((k, k.len() + 1, d, false, false));
        }
    }
    None
}

fn collect_token_word_spans(text: &str, spans: &mut Vec<SensitiveSpan>) {
    let mut offset = 0usize;
    let mut expect_auth_token = false;
    while offset < text.len() {
        let rest = &text[offset..];
        let len = rest
            .find(|c: char| {
                c.is_whitespace()
                    || (!expect_auth_token && c == '=')
                    || matches!(c, '"' | '\'' | '&' | ',' | ';' | '(' | ')' | '<' | '>' | '[' | ']' | '{' | '}')
            })
            .unwrap_or(rest.len());
        if len == 0 {
            let ch = rest.chars().next().unwrap_or('\0');
            if !ch.is_whitespace() {
                expect_auth_token = false;
            }
            offset += ch.len_utf8();
            continue;
        }
        let word = &rest[..len];
        let low = word.to_ascii_lowercase();
        if expect_auth_token {
            if !is_placeholder_or_benign(word) {
                spans.push(SensitiveSpan {
                    start: offset,
                    end: offset + len,
                    kind: SpanKind::Secret,
                    bracketed: false,
                });
            }
            expect_auth_token = false;
        } else if low == "bearer" || low == "basic" {
            expect_auth_token = true;
        } else if is_prefixed_token(word, &low) || is_jwt(word) || looks_opaque_secret(word) {
            spans.push(SensitiveSpan { start: offset, end: offset + len, kind: SpanKind::Secret, bracketed: false });
        }
        offset += len;
    }
}

fn is_prefixed_token(word: &str, lower: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "live_",
        "oauth:",
        "sk_live_",
        "sk_test_",
        "whsec_",
        "ghp_",
        "gho_",
        "ghu_",
        "aiza",
        "xoxb-",
        "xoxp-",
        "fcr_",
        "fca_",
        "age-secret-key-1",
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

fn collect_email_spans(text: &str, spans: &mut Vec<SensitiveSpan>) {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut min_local = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            let mut local_start = i;
            while local_start > min_local && is_email_local(bytes[local_start - 1]) {
                local_start -= 1;
            }
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
                spans.push(SensitiveSpan { start: local_start, end: dom_end, kind: SpanKind::Email, bracketed: false });
                min_local = dom_end;
                i = dom_end;
                continue;
            }
        }
        i += 1;
    }
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

/// Finds `(start_byte, end_byte, is_bracketed)` of every valid IPv4/IPv6 literal (and optional
/// `%zone` suffix). Note `[ipv6]:port` scrubs the `[ipv6]` bracket interior and keeps `:port`.
fn find_ip_spans(text: &str) -> Vec<(usize, usize, bool)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut no_close = false;
    while i < bytes.len() {
        // Bracketed IPv6: `[...]` (may include `%zone`).
        if bytes[i] == b'[' {
            // With no `]` anywhere after this one there can't be one after a later `[` either.
            let found = if no_close { None } else { text[i + 1..].find(']') };
            if found.is_none() {
                no_close = true;
            }
            if let Some(close) = found {
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
            if !left_ok {
                i += 1;
                continue;
            }
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
            let right_ok = end == bytes.len() || !is_ip_boundary_blocker(bytes[end]);
            let candidate = &text[start..end];
            if right_ok && let Some((ip_end_rel, _)) = parse_ip_or_ip_port(candidate) {
                out.push((start, start + ip_end_rel, false));
                continue;
            }
            // If `dead:192.168.1.1` failed as one hex+ip candidate, advance just past the first `:` or `.`.
            if let Some(sep) = candidate.find([':', '.']) {
                i = start + sep + 1;
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
                enumeration_supported: true,
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
    fn redacts_unquoted_kv_with_whitespace_after_separator() {
        let cases: &[(&str, &[&str])] = &[
            ("password: hunter2!", &["hunter2!"]),
            ("token: my_short_tok", &["my_short_tok"]),
            ("secret: s3cr3t", &["s3cr3t"]),
            ("key=   spacedSecret99", &["spacedSecret99"]),
            ("Authorization: CustomScheme rawkey123", &["rawkey123"]),
        ];
        for (raw, forbidden) in cases {
            let red = redact_text(raw, &[], &[]);
            for secret in *forbidden {
                assert!(!red.contains(secret), "secret {secret:?} survived in {red:?} from {raw:?}");
            }
        }
        assert_eq!(redact_text("hotkey=Ctrl+P bypass=true", &[], &[]), "hotkey=Ctrl+P bypass=true");
    }

    #[test]
    fn redacts_query_tokens_headers_escaped_json_pct_ingest_pem_ips_and_spaced_paths() {
        let users = ["John Smith"];
        let cases: &[(&str, &[&str])] = &[
            (
                "GET /api/v1/media/m1?session=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef&ticket=tkt_9988&code=oauth_cb_7766&code_verifier=pkce_ver_55",
                &[
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                    "tkt_9988",
                    "oauth_cb_7766",
                    "pkce_ver_55",
                ],
            ),
            (
                "Cookie: fca_session=sess_cookie_11; csrf_token=csrf_tok_22\nSet-Cookie: sid=set_cookie_33; HttpOnly",
                &["sess_cookie_11", "csrf_tok_22", "set_cookie_33"],
            ),
            (
                "x-api-key: xak_1 x-auth-token: xat_2 x-csrf-token: xct_3 private_key=pk_4 signing_secret=ss_5 webhook_secret=whs_6 pwd=pwd_7 sig=sig_8 signature=sig_9 X-Amz-Signature=amz_10 X-Amz-Security-Token=amz_11",
                &["xak_1", "xat_2", "xct_3", "pk_4", "ss_5", "whs_6", "pwd_7", "sig_8", "sig_9", "amz_10", "amz_11"],
            ),
            (
                r#"{"body":"{\"access_token\":\"esc_tok_999\",\"password\":\"pw\\\"suffix_secret\"}"}"#,
                &["esc_tok_999", "pw", "suffix_secret"],
            ),
            ("{\n  \"access_token\":\n  \"multiline_json_tok\"\n}", &["multiline_json_tok"]),
            (
                "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQE\nSecretPemBodyLine\n-----END PRIVATE KEY-----",
                &["MIIEvQIBADANBgkqhkiG9w0BAQE", "SecretPemBodyLine"],
            ),
            (
                "token%3Dpct_secret_11 url=rtmp%3A%2F%2Flive.twitch.tv%2Fapp%2Fpct_live_key email=pctalice%40example.com C%3A%5CUsers%5CpctWinUser%5CAppData",
                &["pct_secret_11", "pct_live_key", "pctalice", "pctWinUser"],
            ),
            (
                "srtla://ingest.example.com:5000/live/srtla_stream_key rtmpt://h.example/app/rtmpt_k1 rtmpe://h.example/app/rtmpe_k2",
                &["srtla_stream_key", "rtmpt_k1", "rtmpe_k2"],
            ),
            (
                "https://whip.example.com/whip/whip_secret_tok https://rtc.example.com/webRTC/publish/rtc_pub_tok",
                &["whip_secret_tok", "rtc_pub_tok"],
            ),
            (
                "rtmp://live.twitch.tv/app/trailing_slash_key/ rtmp://host.example/app/first_key_part/sub_part rtmp://host.example/app/my_secret_key?cb=a/b",
                &["trailing_slash_key", "first_key_part", "sub_part", "my_secret_key"],
            ),
            (
                "Authorization: Bearer abc=def Basic dXNlcjpwYXNz== fca_se_tok12345 AGE-SECRET-KEY-1QQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQ",
                &["abc=def", "dXNlcjpwYXNz==", "fca_se_tok12345", "AGE-SECRET-KEY-1"],
            ),
            (
                "ip:192.168.1.42 host:10.0.0.5 src:203.0.113.9 dead:192.168.1.1 tcp:203.0.113.10:1935 v6:fe80::1",
                &["192.168.1.42", "10.0.0.5", "203.0.113.9", "192.168.1.1", "203.0.113.10", "fe80::1"],
            ),
            (
                r";C:\Users\John Smith\AppData|C:\Users\Jane Doe\Videos {/Users/BracedUser/data",
                &["John Smith", "Smith", "Jane Doe", "Doe", "BracedUser"],
            ),
        ];

        for (raw, forbidden) in cases {
            let red = redact_text(raw, &[], &users);
            for secret in *forbidden {
                assert!(!red.contains(secret), "secret {secret:?} survived in {red:?} from {raw:?}");
            }
        }

        let safe = "2026-11-22T03:02:11Z INFO ferrocast 1.4.2 Apple M3 Max 48000 Hz status=200 code=404";
        assert_eq!(redact_text(safe, &[], &users), safe);
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
    fn binary_dump_redaction_covers_json_url_kv_prefixed_and_unicode_utf16_runs() {
        let mut blob = Vec::new();
        blob.extend_from_slice(b"MDMP\x93\xa7\x00\x00");
        // ASCII runs with JSON OAuth, webhook, URL userinfo, colon KV, new prefixes, RTMP query, UTF-8 path.
        blob.extend_from_slice(
            concat!(
                r#"{"access_token":"dmp_json_acc_1","refresh_token":"dmp_json_ref_2"} "#,
                "https://discord.com/api/webhooks/123456/dmp_discord_hook_tok ",
                "https://user:dmp_userinfo_pw@example.com/ingest ",
                "password: \"dmp_colon_pw\" ",
                "fca_dmp_tok_11 AGE-SECRET-KEY-1QQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQ ",
                "rtmp://live.twitch.tv/app/dmp_rtmp_cb_key?cb=a/b ",
                "/Users/José/Library/Application Support/Ferrocast ",
            )
            .as_bytes(),
        );
        blob.push(0x00);

        // Even-aligned UTF-16LE run with JSON, colon KV, prefix, RTMP query, and non-ASCII username.
        let u16_even = concat!(
            r#"{"access_token":"u16_json_acc_1"} "#,
            "session_id: u16_sess_99 fca_u16_tok_88 ",
            "rtmp://live.twitch.tv/app/u16_rtmp_key?cb=a/b ",
            r"C:\Users\José\AppData password: u16_after_latin1_pw",
        );
        for u in u16_even.encode_utf16() {
            blob.extend_from_slice(&u.to_le_bytes());
        }
        blob.extend_from_slice(&[0x00, 0x00, 0x7f]); // odd-align next UTF-16LE run

        // Odd-aligned UTF-16LE run with BMP non-Latin1 CJK username and Discord webhook.
        let u16_odd = r"C:\Users\太郎\AppData https://discord.com/api/webhooks/999/u16_odd_hook_tok";
        for u in u16_odd.encode_utf16() {
            blob.extend_from_slice(&u.to_le_bytes());
        }
        let orig_len = blob.len();

        redact_bytes_in_place(&mut blob, &[], &[]);
        assert_eq!(blob.len(), orig_len, "binary RVA layout preserved");

        let as_utf8 = String::from_utf8_lossy(&blob);
        for forbidden in [
            "dmp_json_acc_1",
            "dmp_json_ref_2",
            "dmp_discord_hook_tok",
            "dmp_userinfo_pw",
            "dmp_colon_pw",
            "fca_dmp_tok_11",
            "AGE-SECRET-KEY-1",
            "dmp_rtmp_cb_key",
            "José",
        ] {
            assert!(!as_utf8.contains(forbidden), "ASCII/UTF-8 secret {forbidden} survived in dump");
        }

        for u16_forbidden in [
            "u16_json_acc_1",
            "u16_sess_99",
            "fca_u16_tok_88",
            "u16_rtmp_key",
            "José",
            "u16_after_latin1_pw",
            "太郎",
            "u16_odd_hook_tok",
        ] {
            let pat: Vec<u8> = u16_forbidden.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
            assert!(
                !blob.windows(pat.len()).any(|w| w == pat.as_slice()),
                "UTF-16LE secret {u16_forbidden} survived in dump"
            );
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
            destination_log: Vec::new(),
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

    #[test]
    fn redact_report_scrubs_encoder_labels_and_stream_issues() {
        let mut inp = healthy_input();
        inp.encoders.push(EncoderInfo {
            id: "enc token=enc_secret_id_11".into(),
            label: "NVIDIA NVENC /Users/leakUser99/gpu token=lbl_secret_22".into(),
            hardware: true,
            codec: "H.264 owner@enc.example".into(),
            selected: false,
        });
        inp.last_streams.push(StreamHealthRecord {
            id: "s-1".into(),
            started_at: Millis(1_000),
            ended_at: Some(Millis(2_000)),
            duration_s: 10,
            platforms: vec![Platform::Twitch],
            live: false,
            unclean_end: false,
            fps_target: 60,
            canvas: "1920x1080 rtmp://live.twitch.tv/app/live_canvas_key".into(),
            encoder_id: Some("obs_x264 password=enc_pw_33".into()),
            sample_rate_hz: 48_000,
            dropped_frames: 0,
            skipped_frames: 0,
            lagged_frames: 0,
            rendered_frames: 600,
            avg_fps: 60.0,
            cpu_pct: 20.0,
            memory_mb: 512,
            frame_ms: 16.0,
            destinations: Vec::new(),
            issues: vec!["auth Bearer issue_bearer_tok_44 from 198.51.100.88".into()],
            destination_log: Vec::new(),
        });

        let report = build_support_report(inp, &["opaqueReportSecret"], &["leakUser99"]);
        let json = serde_json::to_string(&report).unwrap();
        for forbidden in [
            "enc_secret_id_11",
            "leakUser99",
            "lbl_secret_22",
            "owner@enc.example",
            "live_canvas_key",
            "enc_pw_33",
            "issue_bearer_tok_44",
            "198.51.100.88",
        ] {
            assert!(!json.contains(forbidden), "field leaked {forbidden:?} into SupportReport: {json}");
        }
    }

    #[test]
    fn unsupported_audio_enumeration_does_not_flag_no_audio_input() {
        let mut inp = healthy_input();
        inp.audio.devices.clear();
        inp.audio.enumeration_supported = false;
        let report = build_support_report(inp, &[], &[]);
        assert!(
            !report.likely_causes.iter().any(|c| c.code == "no_audio_input"),
            "unexpected no_audio_input on unsupported OS: {:?}",
            report.likely_causes
        );
        let summary = format_summary_txt(&report, &[], &[]);
        assert!(summary.contains("enumeration not supported"));

        let mut supported_empty = healthy_input();
        supported_empty.audio.devices.clear();
        supported_empty.audio.enumeration_supported = true;
        let supported_report = build_support_report(supported_empty, &[], &[]);
        assert!(supported_report.likely_causes.iter().any(|c| c.code == "no_audio_input"));
    }

    #[test]
    fn odd_log_endings_never_panic() {
        for tail in ["connect to http://", "ws://", "x ://", "://", "https://\n"] {
            let _ = redact_text(tail, &[], &[]);
        }
        // A private-key header at the very end of a log (and with only a newline after it).
        for tail in [
            "log\n-----BEGIN PRIVATE KEY-----",
            "log\n-----BEGIN PRIVATE KEY-----\n",
            "-----BEGIN RSA PRIVATE KEY-----\r\n",
        ] {
            let _ = redact_text(tail, &[], &[]);
        }
        // A known username whose case-folded form is shorter than itself (the Kelvin sign).
        let _ = redact_text("kim kim", &[], &["\u{212A}im"]);
    }

    #[test]
    fn a_percent_encoded_known_secret_is_redacted() {
        let secret = "abc-DEF-12345";
        let out = redact_text("seen value abc%2dDEF%2d12345 in a url path", &[secret], &[]);
        assert!(!out.contains("abc%2dDEF"), "{out}");
        assert!(!out.contains("DEF%2d12345"), "{out}");
    }

    #[test]
    fn a_huge_line_is_redacted_in_linear_time() {
        let big = "a".repeat(1 << 20);
        let t = std::time::Instant::now();
        let out = redact_text(&big, &["supersecret"], &["someuser"]);
        assert_eq!(out.len(), big.len());
        assert!(t.elapsed() < std::time::Duration::from_secs(20), "{:?}", t.elapsed());
    }

    #[test]
    fn many_open_brackets_do_not_make_the_ip_scan_quadratic() {
        let text = "[".repeat(400_000);
        let t = std::time::Instant::now();
        let _ = redact_text(&text, &[], &[]);
        assert!(t.elapsed() < std::time::Duration::from_secs(10), "{:?}", t.elapsed());
        // Still finds an address after a run of stray brackets.
        let out = redact_text("[[[[ see [2001:db8::1] done", &[], &[]);
        assert!(!out.contains("2001:db8::1"), "{out}");
    }
}
