//! One-click support / diagnostic bundle builder (`#7`) and local stream-health history.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use ferrocast_core::diagnostics::{
    AudioDeviceInfo, AudioSummary, ConnectorDiagnostic, CrashDumpSummary, DestinationHealthSnapshot, DiagnosticInput,
    EncoderInfo, GpuInfo, OsVersionInfo, StreamHealthRecord, SupportReport, build_support_report, format_summary_txt,
    redact_bytes_in_place, redact_text,
};
use ferrocast_core::event::{Millis, Platform};
use ferrocast_store::repo;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Engine, EngineError, now, platform_key};

const MAX_LOG_LINES_PER_FILE: usize = 2_500;
const MAX_LOG_BYTES_PER_FILE: u64 = 1024 * 1024;
const MAX_CRASH_FILES: usize = 5;
const MAX_TEXT_CRASH_BYTES: u64 = 2 * 1024 * 1024;
const MAX_BINARY_DUMP_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TOTAL_CRASH_ZIP_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Default)]
pub(crate) struct ActiveStreamHealth {
    active: Option<StreamHealthRecord>,
}

/// Result returned when writing a support archive to a user-selected folder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WrittenReport {
    pub path: String,
    pub filename: String,
    pub size_bytes: u64,
    pub report: SupportReport,
}

impl Engine {
    /// Marks any still-live stream health record as unclean if the previous process crashed.
    pub(crate) async fn mark_unclean_stream_health(&self) {
        if self.safe_mode_status().unclean_exit
            && let Err(e) = self.db(repo::stream_health::mark_unended_as_unclean).await
        {
            tracing::warn!(error = %e, "couldn't update unended stream health record");
        }
    }

    /// Explicitly stores/updates a stream health snapshot (retains newest 5 locally).
    pub async fn record_stream_health(&self, record: StreamHealthRecord) -> Result<(), EngineError> {
        self.db(move |s| repo::stream_health::record(s, record)).await
    }

    pub(crate) async fn health_stream_started(&self, at: Millis, platform: Platform) {
        let studio = match self.studio_host() {
            Ok(h) => h.diagnostics_studio().await,
            Err(_) => None,
        };
        let canvas =
            studio.as_ref().and_then(|v| v.get("canvas").and_then(Value::as_str)).unwrap_or("1920x1080").to_string();
        let fps_target = studio.as_ref().and_then(|v| v.get("fps").and_then(Value::as_u64)).unwrap_or(60) as u32;
        let encoder_id = studio.as_ref().and_then(|v| v.get("video_encoder").and_then(Value::as_str)).map(String::from);
        let sample_rate_hz =
            studio.as_ref().and_then(|v| v.get("sample_rate").and_then(Value::as_u64)).unwrap_or(48_000) as u32;

        let rec = {
            let mut guard = self.inner.stream_health.lock().unwrap_or_else(|p| p.into_inner());
            let rec = guard.active.get_or_insert_with(|| StreamHealthRecord {
                id: format!("stream-{}", at.0),
                started_at: at,
                ended_at: None,
                duration_s: 0,
                platforms: vec![],
                live: true,
                unclean_end: false,
                fps_target,
                canvas,
                encoder_id,
                sample_rate_hz,
                dropped_frames: 0,
                skipped_frames: 0,
                lagged_frames: 0,
                rendered_frames: 0,
                avg_fps: f64::from(fps_target),
                cpu_pct: 0.0,
                memory_mb: 0,
                frame_ms: 0.0,
                destinations: vec![],
                issues: vec![],
            });
            if !rec.platforms.contains(&platform) {
                rec.platforms.push(platform);
            }
            rec.clone()
        };
        let _ = self.db(move |s| repo::stream_health::record(s, rec)).await;
    }

    pub(crate) async fn health_stream_ended(&self, at: Millis) {
        let mut rec = {
            let mut guard = self.inner.stream_health.lock().unwrap_or_else(|p| p.into_inner());
            guard.active.take()
        };
        if let Some(r) = &mut rec {
            r.live = false;
            r.ended_at = Some(at);
            r.duration_s = u64::try_from((at.0 - r.started_at.0).max(0) / 1000).unwrap_or(0);
            self.populate_live_metrics(r).await;
            let to_save = r.clone();
            let _ = self.db(move |s| repo::stream_health::record(s, to_save)).await;
        }
    }

    async fn populate_live_metrics(&self, r: &mut StreamHealthRecord) {
        use ferrocast_golive::PushState;

        let gv = self.golive_view().await;
        let mut dests = Vec::new();
        for d in &gv.destinations {
            if !d.enabled {
                continue;
            }
            let h = d.health.as_ref();
            let (st_label, reconnects, last_error) = match h.map(|x| &x.state) {
                Some(PushState::Live) => ("live", 0, None),
                Some(PushState::Connecting) => ("connecting", 0, None),
                Some(PushState::Waiting) => ("waiting", 0, None),
                Some(PushState::Stopped) => ("stopped", 0, None),
                Some(PushState::Reconnecting { attempt, reason, .. }) => {
                    ("reconnecting", *attempt, Some(reason.clone()))
                }
                Some(PushState::Failed { attempts, reason, .. }) => ("failed", *attempts, Some(reason.clone())),
                Some(PushState::Paused { reason, .. }) => ("paused", 0, Some(reason.clone())),
                None => ("off", 0, None),
            };
            let dropped = h.map_or(0, |x| x.dropped);
            let sent = h.map_or(0, |x| x.sent);
            let platform_name = match d.kind {
                ferrocast_golive::Service::Twitch => "twitch",
                ferrocast_golive::Service::Youtube => "youtube",
                ferrocast_golive::Service::Kick => "kick",
                ferrocast_golive::Service::Custom => "custom",
            };
            dests.push(DestinationHealthSnapshot {
                platform: platform_name.into(),
                target: d.account.clone().unwrap_or_else(|| d.name.clone()),
                state: st_label.into(),
                kbps: h.map_or(0, |x| x.kbps),
                dropped_frames: dropped,
                total_frames: sent,
                rtt_ms: h.and_then(|x| x.link.as_ref().map(|l| l.rtt_ms)),
                lost_pct: h.and_then(|x| x.link.as_ref().map(|l| l.lost_pct)),
                reconnects,
                last_error,
            });
            r.dropped_frames = r.dropped_frames.max(dropped);
        }
        r.destinations = dests;

        if let Ok(host) = self.studio_host()
            && let Some(studio) = host.diagnostics_studio().await
            && let Some(stats) = studio.get("stats")
        {
            if let Some(fps) = stats.get("fps").and_then(Value::as_f64) {
                r.avg_fps = fps;
            }
            if let Some(ms) = stats.get("frame_ms").and_then(Value::as_f64) {
                r.frame_ms = ms;
            }
            if let Some(cpu) = stats.get("cpu").and_then(Value::as_f64) {
                r.cpu_pct = cpu as f32;
            }
            if let Some(mem) = stats.get("memory_mb").and_then(Value::as_u64) {
                r.memory_mb = mem;
            }
            if let Some(lag) = stats.get("lagged").and_then(Value::as_u64) {
                r.lagged_frames = r.lagged_frames.max(lag);
            }
            if let Some(skip) = stats.get("skipped").and_then(Value::as_u64) {
                r.skipped_frames = r.skipped_frames.max(skip);
            }
            if let Some(frames) = stats.get("frames").and_then(Value::as_u64) {
                r.rendered_frames = r.rendered_frames.max(frames);
            }
        }
    }

    /// Returns a redacted diagnostic preview (without creating a zip on disk).
    pub async fn diagnostics_preview(&self) -> Result<SupportReport, EngineError> {
        let (input, secrets, users, _logs, _crashes) = self.gather_diagnostics().await?;
        let s_refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
        let u_refs: Vec<&str> = users.iter().map(String::as_str).collect();
        Ok(build_support_report(input, &s_refs, &u_refs))
    }

    /// Builds the complete redacted support zip archive in memory (`(filename, zip_bytes, report)`).
    pub async fn diagnostics_zip_bytes(&self) -> Result<(String, Vec<u8>, SupportReport), EngineError> {
        let (input, secrets, users, logs, crash_files) = self.gather_diagnostics().await?;
        let s_refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
        let u_refs: Vec<&str> = users.iter().map(String::as_str).collect();

        let report = build_support_report(input, &s_refs, &u_refs);
        let summary_txt = format_summary_txt(&report, &s_refs, &u_refs);
        let report_json_raw = serde_json::to_string_pretty(&report)
            .map_err(|e| EngineError::Unavailable(format!("Couldn't serialize support report: {e}")))?;
        let report_json = redact_text(&report_json_raw, &s_refs, &u_refs);

        let filename = format!("ferrocast-support-{}.zip", format_utc_stamp(report.generated_at));
        let cursor = std::io::Cursor::new(Vec::new());
        let mut zw = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

        zw.start_file("SUMMARY.txt", opts).map_err(zip_err)?;
        zw.write_all(summary_txt.as_bytes()).map_err(zip_err)?;

        zw.start_file("report.json", opts).map_err(zip_err)?;
        zw.write_all(report_json.as_bytes()).map_err(zip_err)?;

        for (name, body) in logs {
            let safe_name = sanitize_zip_component(&redact_text(&name, &s_refs, &u_refs));
            zw.start_file(format!("logs/{safe_name}"), opts).map_err(zip_err)?;
            zw.write_all(body.as_bytes()).map_err(zip_err)?;
        }

        for (name, bytes) in crash_files {
            let safe_name = sanitize_zip_component(&redact_text(&name, &s_refs, &u_refs));
            zw.start_file(format!("crashes/{safe_name}"), opts).map_err(zip_err)?;
            zw.write_all(&bytes).map_err(zip_err)?;
        }

        let buf = zw.finish().map_err(zip_err)?.into_inner();
        Ok((filename, buf, report))
    }

    /// Writes the redacted support report zip into `folder`.
    pub async fn write_diagnostics_report(&self, folder: &Path) -> Result<WrittenReport, EngineError> {
        let canonical = tokio::task::spawn_blocking({
            let f = folder.to_path_buf();
            move || -> Result<PathBuf, EngineError> {
                if f.as_os_str().is_empty() {
                    return Err(EngineError::Invalid("Pick a destination folder first.".into()));
                }
                if !f.exists() {
                    return Err(EngineError::Invalid("That folder does not exist.".into()));
                }
                if !f.is_dir() {
                    return Err(EngineError::Invalid("Pick a folder, not a file.".into()));
                }
                Ok(f)
            }
        })
        .await
        .map_err(|e| EngineError::Unavailable(e.to_string()))??;

        let (filename, bytes, report) = self.diagnostics_zip_bytes().await?;
        let dest = canonical.join(&filename);
        let size_bytes = u64::try_from(bytes.len()).unwrap_or(0);
        let out_path = dest.clone();

        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let tmp = out_path.with_extension("zip.part");
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, &out_path)?;
            Ok(())
        })
        .await
        .map_err(|e| EngineError::Unavailable(e.to_string()))?
        .map_err(|e| EngineError::Unavailable(format!("Couldn't save the support archive here: {e}")))?;

        Ok(WrittenReport { path: dest.display().to_string(), filename, size_bytes, report })
    }

    async fn gather_diagnostics(
        &self,
    ) -> Result<(DiagnosticInput, Vec<String>, Vec<String>, Vec<(String, String)>, Vec<(String, Vec<u8>)>), EngineError>
    {
        let generated_at = now();
        let data_dir = self.inner.data_dir.clone();
        let known_secrets = self.collect_known_secrets();
        let known_usernames = collect_known_usernames(&data_dir);
        let s_refs: Vec<&str> = known_secrets.iter().map(String::as_str).collect();
        let u_refs: Vec<&str> = known_usernames.iter().map(String::as_str).collect();

        let studio = match self.studio_host() {
            Ok(h) => h.diagnostics_studio().await,
            Err(_) => None,
        };

        let mut last_streams = self.db(|s| repo::stream_health::last(s, 5)).await.unwrap_or_default();
        // Include live active session updates at the top if currently live.
        let active_opt = self.inner.stream_health.lock().unwrap_or_else(|p| p.into_inner()).active.clone();
        if let Some(mut live_rec) = active_opt {
            self.populate_live_metrics(&mut live_rec).await;
            if let Some(pos) = last_streams.iter().position(|x| x.id == live_rec.id) {
                last_streams[pos] = live_rec;
            } else {
                last_streams.insert(0, live_rec);
                last_streams.truncate(5);
            }
        }

        let safe_status = self.safe_mode_status();
        let connectors: Vec<ConnectorDiagnostic> = {
            let st = self.state();
            [Platform::Twitch, Platform::Youtube, Platform::Kick]
                .into_iter()
                .filter_map(|p| {
                    let key = platform_key(p);
                    st.connectors.get(key).map(|c| {
                        let state_str = serde_json::to_value(&c.state)
                            .ok()
                            .and_then(|v| v.as_str().map(String::from))
                            .unwrap_or_else(|| "off".into());
                        ConnectorDiagnostic { platform: p, state: state_str, detail: c.detail.clone() }
                    })
                })
                .collect()
        };

        // Hardware & filesystem probes off the async executor.
        let s_owned = known_secrets.clone();
        let u_owned = known_usernames.clone();
        let studio_for_probe = studio.clone();
        let data_for_probe = data_dir.clone();

        let (os, gpu, audio_devices, disk_free_mb, logs, recent_errors, crash_meta, crash_files) =
            tokio::task::spawn_blocking(move || {
                let s_refs: Vec<&str> = s_owned.iter().map(String::as_str).collect();
                let u_refs: Vec<&str> = u_owned.iter().map(String::as_str).collect();

                let os = OsVersionInfo {
                    name: sysinfo::System::name().unwrap_or_else(|| std::env::consts::OS.into()),
                    version: sysinfo::System::os_version().unwrap_or_default(),
                    long_version: sysinfo::System::long_os_version().unwrap_or_default(),
                    kernel_version: sysinfo::System::kernel_version().unwrap_or_default(),
                };

                let probe_gpu = ferrocore::diagnostics_probe::probe_gpu();
                let gpu = GpuInfo {
                    name: probe_gpu.name,
                    backend: probe_gpu.backend,
                    device_type: probe_gpu.device_type,
                    driver: probe_gpu.driver,
                    driver_info: probe_gpu.driver_info,
                    is_software: probe_gpu.is_software,
                    active_renderer: studio_for_probe
                        .as_ref()
                        .and_then(|v| v.get("renderer").and_then(Value::as_str).map(String::from)),
                };

                let audio_devices: Vec<AudioDeviceInfo> = ferrocore::diagnostics_probe::probe_audio_devices()
                    .into_iter()
                    .map(|d| AudioDeviceInfo {
                        kind: d.kind,
                        name: d.name,
                        is_default: d.is_default,
                        sample_rate_hz: d.sample_rate_hz,
                        supported_sample_rates_hz: d.supported_sample_rates_hz,
                        channels: d.channels,
                    })
                    .collect();

                let disk_free_mb = free_disk_mb(&data_for_probe);
                let (logs, recent_errors) = gather_redacted_logs(&data_for_probe.join("logs"), &s_refs, &u_refs);
                let (crash_meta, crash_files) =
                    gather_redacted_crashes(&data_for_probe.join("crashes"), &s_refs, &u_refs);

                (os, gpu, audio_devices, disk_free_mb, logs, recent_errors, crash_meta, crash_files)
            })
            .await
            .map_err(|e| EngineError::Unavailable(e.to_string()))?;

        let selected_encoder =
            studio.as_ref().and_then(|v| v.get("video_encoder").and_then(Value::as_str).map(String::from));
        let encoders = build_encoders_list(studio.as_ref(), selected_encoder.as_deref());
        let studio_sample_rate_hz =
            studio.as_ref().and_then(|v| v.get("sample_rate").and_then(Value::as_u64)).unwrap_or(48_000) as u32;
        let monitoring_device =
            studio.as_ref().and_then(|v| v.get("monitoring").and_then(Value::as_str).map(String::from));

        let logs_included: Vec<String> =
            logs.iter().map(|(n, _)| redact_text(&format!("logs/{n}"), &s_refs, &u_refs)).collect();

        let input = DiagnosticInput {
            generated_at,
            app: ferrocast_core::diagnostics::AppVersionInfo {
                version: env!("CARGO_PKG_VERSION").into(),
                os: std::env::consts::OS.into(),
                arch: std::env::consts::ARCH.into(),
                build_mode: if cfg!(debug_assertions) { "debug".into() } else { "release".into() },
            },
            os,
            gpu,
            encoders,
            configured_encoder_id: selected_encoder,
            audio: AudioSummary { studio_sample_rate_hz, monitoring_device, devices: audio_devices },
            last_streams,
            crashes: crash_meta,
            connectors,
            recent_log_errors: recent_errors,
            logs_included,
            disk_free_mb,
            safe_mode: safe_status.active,
            unclean_exit: safe_status.unclean_exit,
        };

        Ok((input, known_secrets, known_usernames, logs, crash_files))
    }

    fn collect_known_secrets(&self) -> Vec<String> {
        let mut out = HashSet::new();
        let mut keys: Vec<String> = vec![
            "twitch-tokens".into(),
            "youtube-tokens".into(),
            "kick-tokens".into(),
            "obs-password".into(),
            "discord-webhook".into(),
            "relay-install".into(),
            "ferrocast-session".into(),
            "backup-key".into(),
            "golive-ingest-key".into(),
            "golive-obs-before".into(),
            "golive-vertical-before".into(),
            "tips-secret".into(),
            "stripe-secret".into(),
            "razorpay-secret".into(),
        ];
        for d in self.golive_config().destinations {
            keys.push(format!("golive-custom-key:{}", d.id));
        }
        for key in keys {
            if let Ok(Some(val)) = self.inner.secrets.get(&key) {
                extract_secret_strings(&val, &mut out);
            }
        }
        out.into_iter().collect()
    }
}

fn extract_secret_strings(raw: &str, out: &mut HashSet<String>) {
    let t = raw.trim();
    if t.len() >= 4 {
        if let Ok(v) = serde_json::from_str::<Value>(t) {
            extract_json_secrets(&v, out);
        } else {
            out.insert(t.to_string());
        }
    }
}

fn extract_json_secrets(v: &Value, out: &mut HashSet<String>) {
    match v {
        Value::String(s) => {
            if s.len() >= 4 {
                out.insert(s.clone());
            }
        }
        Value::Array(arr) => {
            for item in arr {
                extract_json_secrets(item, out);
            }
        }
        Value::Object(map) => {
            for val in map.values() {
                extract_json_secrets(val, out);
            }
        }
        _ => {}
    }
}

fn collect_known_usernames(data_dir: &Path) -> Vec<String> {
    let mut set = HashSet::new();
    for var in ["USER", "USERNAME", "LOGNAME"] {
        if let Ok(v) = std::env::var(var)
            && v.trim().len() >= 3
        {
            set.insert(v.trim().to_string());
        }
    }
    // Also inspect `/Users/<name>`, `/home/<name>`, or `\Users\<name>` in data_dir itself.
    let s = data_dir.to_string_lossy();
    let lower = s.to_ascii_lowercase();
    for marker in ["/users/", "/home/", "\\users\\"] {
        if let Some(idx) = lower.find(marker) {
            let after = &s[idx + marker.len()..];
            if let Some(seg) = after.split(['/', '\\']).next()
                && seg.len() >= 3
            {
                set.insert(seg.to_string());
            }
        }
    }
    set.into_iter().collect()
}

fn build_encoders_list(studio: Option<&Value>, selected: Option<&str>) -> Vec<EncoderInfo> {
    if let Some(list) = studio.and_then(|v| v.get("encoders").and_then(Value::as_array))
        && !list.is_empty()
    {
        return list
            .iter()
            .filter_map(|item| {
                let id = item.get("id").and_then(Value::as_str)?.to_string();
                let label = item.get("name").and_then(Value::as_str).unwrap_or(&id).to_string();
                let hardware = item.get("hardware").and_then(Value::as_bool).unwrap_or_else(|| !id.contains("x264"));
                let codec = if id.to_ascii_lowercase().contains("hevc") { "HEVC".into() } else { "H.264".into() };
                Some(EncoderInfo { selected: selected == Some(id.as_str()), id, label, hardware, codec })
            })
            .collect();
    }

    if cfg!(target_os = "macos") {
        vec![
            EncoderInfo {
                id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
                label: "Apple VT H264 Hardware Encoder".into(),
                hardware: true,
                codec: "H.264".into(),
                selected: selected == Some("com.apple.videotoolbox.videoencoder.ave.avc"),
            },
            EncoderInfo {
                id: "com.apple.videotoolbox.videoencoder.ave.hevc".into(),
                label: "Apple VT HEVC Hardware Encoder".into(),
                hardware: true,
                codec: "HEVC".into(),
                selected: selected == Some("com.apple.videotoolbox.videoencoder.ave.hevc"),
            },
            EncoderInfo {
                id: "obs_x264".into(),
                label: "x264".into(),
                hardware: false,
                codec: "H.264".into(),
                selected: selected == Some("obs_x264"),
            },
        ]
    } else {
        vec![
            EncoderInfo {
                id: "ffmpeg_hardware".into(),
                label: "Hardware (NVENC, AMF or Quick Sync)".into(),
                hardware: true,
                codec: "H.264".into(),
                selected: selected == Some("ffmpeg_hardware"),
            },
            EncoderInfo {
                id: "obs_x264".into(),
                label: "x264".into(),
                hardware: false,
                codec: "H.264".into(),
                selected: selected == Some("obs_x264"),
            },
        ]
    }
}

fn free_disk_mb(path: &Path) -> Option<u64> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let best = disks
        .list()
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len());
    best.map(|d| d.available_space() / (1024 * 1024))
}

fn gather_redacted_logs(logs_dir: &Path, secrets: &[&str], users: &[&str]) -> (Vec<(String, String)>, Vec<String>) {
    let mut files_out = Vec::new();
    let mut errors = Vec::new();
    for fname in ["ferrocast.1.log", "ferrocast.log"] {
        let p = logs_dir.join(fname);
        if !p.is_file() {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&p) else { continue };
        let Ok(raw_bytes) = read_tail_bytes(&p, meta.len(), MAX_LOG_BYTES_PER_FILE) else { continue };
        let text = String::from_utf8_lossy(&raw_bytes);
        let all_lines: Vec<String> = text.lines().map(strip_ansi).collect();
        let start = all_lines.len().saturating_sub(MAX_LOG_LINES_PER_FILE);
        let joined = all_lines[start..].join("\n");
        let redacted = redact_text(&joined, secrets, users);
        for l in redacted.lines() {
            if is_error_line(l) && errors.len() < 20 {
                errors.push(l.to_string());
            }
        }
        files_out.push((fname.to_string(), redacted));
    }
    (files_out, errors)
}

fn read_tail_bytes(path: &Path, len: u64, cap: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    if len > cap {
        f.seek(SeekFrom::Start(len - cap))?;
    }
    let mut buf = Vec::with_capacity(len.min(cap) as usize);
    f.read_to_end(&mut buf)?;
    // Drop partial leading line after seek
    if len > cap
        && let Some(pos) = buf.iter().position(|&b| b == b'\n')
    {
        return Ok(buf[pos + 1..].to_vec());
    }
    Ok(buf)
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            for d in chars.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn is_error_line(l: &str) -> bool {
    l.contains(" ERROR ")
        || l.starts_with("ERROR ")
        || l.contains("panicked")
        || l.contains("stopped unexpectedly")
        || l.contains("OutOfMemory")
        || l.contains("DeviceLost")
}

fn gather_redacted_crashes(
    crashes_dir: &Path,
    secrets: &[&str],
    users: &[&str],
) -> (Vec<CrashDumpSummary>, Vec<(String, Vec<u8>)>) {
    let Ok(rd) = std::fs::read_dir(crashes_dir) else {
        return (Vec::new(), Vec::new());
    };
    let mut entries: Vec<(std::path::PathBuf, std::fs::Metadata)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            let m = e.metadata().ok()?;
            m.is_file().then_some((p, m))
        })
        .collect();
    // Newest first
    entries.sort_by(|a, b| {
        let ta = a.1.modified().ok();
        let tb = b.1.modified().ok();
        tb.cmp(&ta).then_with(|| a.0.file_name().cmp(&b.0.file_name()))
    });

    let mut meta_out = Vec::new();
    let mut files_out = Vec::new();
    let mut total_bytes: u64 = 0;

    for (path, meta) in entries.into_iter().take(MAX_CRASH_FILES) {
        let raw_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("crash");
        let name = redact_text(raw_name, secrets, users);
        let modified_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("").to_ascii_lowercase();
        let is_binary_dmp = ext == "dmp";

        let mut included = false;
        let mut summary = None;

        if total_bytes < MAX_TOTAL_CRASH_ZIP_BYTES {
            let limit = if is_binary_dmp { MAX_BINARY_DUMP_BYTES } else { MAX_TEXT_CRASH_BYTES };
            if meta.len() <= limit
                && total_bytes.saturating_add(meta.len()) <= MAX_TOTAL_CRASH_ZIP_BYTES
                && let Ok(mut bytes) = std::fs::read(&path)
            {
                if is_binary_dmp {
                    let is_mdmp = bytes.starts_with(b"MDMP");
                    redact_bytes_in_place(&mut bytes, secrets, users);
                    summary = Some(if is_mdmp {
                        "Windows minidump (redacted)".into()
                    } else {
                        "Binary dump (redacted)".into()
                    });
                } else {
                    let text = String::from_utf8_lossy(&bytes);
                    let red = redact_text(&text, secrets, users);
                    bytes = red.into_bytes();
                }
                total_bytes = total_bytes.saturating_add(u64::try_from(bytes.len()).unwrap_or(0));
                files_out.push((name.clone(), bytes));
                included = true;
            }
        }

        meta_out.push(CrashDumpSummary {
            filename: name,
            modified_ms,
            size_bytes: meta.len(),
            kind: if is_binary_dmp { "minidump".into() } else { "log".into() },
            included_in_zip: included,
            summary,
        });
    }

    (meta_out, files_out)
}

fn sanitize_zip_component(name: &str) -> String {
    let cleaned: String = name.chars().map(|c| if matches!(c, '/' | '\\' | '\0' | ':') { '_' } else { c }).collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." { "file.txt".into() } else { cleaned }
}

fn format_utc_stamp(ts: Millis) -> String {
    let secs = ts.0.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let hour = tod / 3600;
    let min = (tod % 3600) / 60;
    let sec = tod % 60;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = y + i64::from(m <= 2);
    format!("{year:04}{m:02}{d:02}-{hour:02}{min:02}{sec:02}")
}

fn zip_err(e: impl std::fmt::Display) -> EngineError {
    EngineError::Unavailable(format!("Couldn't build support zip archive: {e}"))
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use ferrocast_core::diagnostics::DestinationHealthSnapshot;
    use ferrocast_secrets::{MemorySecrets, Secrets};

    use super::*;

    #[tokio::test]
    async fn real_support_zip_scrubs_every_fixture_secret_and_lists_causes_at_top() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        let crashes = dir.path().join("crashes");
        let out_folder = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::create_dir_all(&crashes).unwrap();

        let secrets = std::sync::Arc::new(MemorySecrets::default());
        secrets
            .set("twitch-tokens", r#"{"access_token":"tok_secret_fixture_alpha","refresh_token":"ref_fixture_beta"}"#)
            .unwrap();
        secrets.set("obs-password", "obsPassFixture_4455").unwrap();
        secrets.set("discord-webhook", "https://discord.com/api/webhooks/111222333/webhookSecretToken_7788").unwrap();
        secrets.set("golive-ingest-key", "live_fixture_streamkey_0099").unwrap();

        let engine = Engine::start_with_secrets(dir.path(), secrets).await.unwrap();

        // Populate a failing recent stream health record
        engine
            .record_stream_health(StreamHealthRecord {
                id: "stream-bad-1".into(),
                started_at: Millis(1_700_000_000_000),
                ended_at: Some(Millis(1_700_000_600_000)),
                duration_s: 600,
                platforms: vec![Platform::Twitch],
                live: false,
                unclean_end: false,
                fps_target: 60,
                canvas: "1920x1080".into(),
                encoder_id: Some("obs_x264".into()),
                sample_rate_hz: 48_000,
                dropped_frames: 240,
                skipped_frames: 150,
                lagged_frames: 120,
                rendered_frames: 3000,
                avg_fps: 49.5,
                cpu_pct: 91.0,
                memory_mb: 1600,
                frame_ms: 24.0,
                destinations: vec![DestinationHealthSnapshot {
                    platform: "twitch".into(),
                    target: "rtmp://live.twitch.tv/app/live_fixture_streamkey_0099".into(),
                    state: "failed".into(),
                    kbps: 6000,
                    dropped_frames: 240,
                    total_frames: 3000,
                    rtt_ms: Some(180),
                    lost_pct: Some(8.0),
                    reconnects: 3,
                    last_error: Some("auth Bearer jwtFixtureSecret999 failed from 203.0.113.77".into()),
                }],
                issues: vec!["drop".into()],
            })
            .await
            .unwrap();

        // Plant fixture logs containing every sensitive pattern
        let dirty_log = concat!(
            "2026-11-22T03:00:00Z INFO starting at /Users/secretHomeUser99/Ferrocast\n",
            "2026-11-22T03:00:01Z WARN connecting rtmp://live.twitch.tv/app/live_fixture_streamkey_0099\n",
            "2026-11-22T03:00:02Z ERROR user=secret.email@fixture.example ip=198.51.100.42 v6=[2001:db8::99]:47820\n",
            "2026-11-22T03:00:03Z ERROR password=obsPassFixture_4455 token=tok_secret_fixture_alpha path=C:\\Users\\WinFixtureUser\\Videos\\out.mkv\n",
            "2026-11-22T03:00:04Z ERROR webhook=https://discord.com/api/webhooks/111222333/webhookSecretToken_7788\n",
        );
        std::fs::write(logs.join("ferrocast.log"), dirty_log).unwrap();

        // Plant both text crash notes & a binary MDMP crash dump with ASCII + UTF-16LE fixture secrets
        std::fs::write(
            crashes.join("panic-note.txt"),
            "panic for owner@crash.example at /home/linuxCrashUser/ferrocast token=ref_fixture_beta",
        )
        .unwrap();
        let mut dmp = b"MDMP\x93\xa7\x00\x00".to_vec();
        dmp.extend_from_slice(b"stream_key=live_fixture_streamkey_0099 email=bin@dmp.example ");
        for u in "C:\\Users\\BinDumpUser\\AppData password=obsPassFixture_4455 192.0.2.55".encode_utf16() {
            dmp.extend_from_slice(&u.to_le_bytes());
        }
        std::fs::write(crashes.join("ferrocast 3.4.0 2026-11-22 03-00-00.dmp"), &dmp).unwrap();

        let written = engine.write_diagnostics_report(out_folder.path()).await.unwrap();
        let zip_path = PathBuf::from(&written.path);
        assert!(zip_path.is_file());
        assert!(!written.report.likely_causes.is_empty());

        let file = std::fs::File::open(&zip_path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();

        let mut names = Vec::new();
        let forbidden_ascii = [
            "tok_secret_fixture_alpha",
            "ref_fixture_beta",
            "obsPassFixture_4455",
            "webhookSecretToken_7788",
            "live_fixture_streamkey_0099",
            "secretHomeUser99",
            "WinFixtureUser",
            "linuxCrashUser",
            "BinDumpUser",
            "secret.email@fixture.example",
            "owner@crash.example",
            "bin@dmp.example",
            "198.51.100.42",
            "203.0.113.77",
            "192.0.2.55",
            "2001:db8::99",
            "jwtFixtureSecret999",
        ];

        for idx in 0..archive.len() {
            let mut entry = archive.by_index(idx).unwrap();
            let entry_name = entry.name().unwrap().into_owned();
            let mut raw = Vec::new();
            entry.read_to_end(&mut raw).unwrap();
            let as_lossy = String::from_utf8_lossy(&raw);
            for secret in forbidden_ascii {
                assert!(
                    !as_lossy.contains(secret),
                    "ASCII fixture secret {secret:?} leaked in zip entry {entry_name}: {as_lossy}"
                );
                let u16_pat: Vec<u8> = secret.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
                assert!(
                    !raw.windows(u16_pat.len()).any(|w| w == u16_pat.as_slice()),
                    "UTF-16LE fixture secret {secret:?} leaked in zip entry {entry_name}"
                );
            }
            if entry_name == "SUMMARY.txt" {
                let causes_i = as_lossy.find("=== LIKELY CAUSES ===").unwrap();
                let versions_i = as_lossy.find("=== APP & OS VERSIONS ===").unwrap();
                assert!(causes_i < versions_i);
                assert!(as_lossy.contains("=== LAST 5 STREAMS HEALTH ==="));
                assert!(as_lossy.contains("=== AUDIO DEVICES & SAMPLE RATES ==="));
                assert!(as_lossy.contains("=== GPU & ENCODERS ==="));
            }
            names.push(entry_name);
        }

        assert!(names.contains(&"SUMMARY.txt".to_string()));
        assert!(names.contains(&"report.json".to_string()));
        assert!(names.contains(&"logs/ferrocast.log".to_string()));
        assert!(names.contains(&"crashes/panic-note.txt".to_string()));
        assert!(names.iter().any(|n| n.starts_with("crashes/") && n.ends_with(".dmp")));
    }

    #[tokio::test]
    async fn support_zip_redacts_multiline_pem_and_json_logs() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join("ferrocast.log"),
            concat!(
                "2026-11-22T03:00:00Z ERROR payload={\n",
                "  \"access_token\":\n",
                "    \"short_ml_tok\"\n",
                "}\n",
                "-----BEGIN PRIVATE KEY-----\n",
                "MIIEvQIBADANBgkqhkiG9w0BAQE\n",
                "PemLogSecretLineABC\n",
                "-----END PRIVATE KEY-----\n",
            ),
        )
        .unwrap();

        let engine = Engine::start_with_memory_secrets(dir.path()).await.unwrap();
        let (_name, bytes, _report) = engine.diagnostics_zip_bytes().await.unwrap();
        let mut arc = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut log_body = String::new();
        arc.by_name("logs/ferrocast.log").unwrap().read_to_string(&mut log_body).unwrap();
        assert!(!log_body.contains("short_ml_tok"), "multiline JSON leaked: {log_body}");
        assert!(!log_body.contains("PemLogSecretLineABC"), "PEM block leaked: {log_body}");
    }
}
