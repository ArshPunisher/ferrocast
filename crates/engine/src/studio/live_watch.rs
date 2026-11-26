//! The live watch: every two seconds while the Ferrocast Studio streams, a look at what goes out
//! (the show's mics and cameras, Go live's destinations), fed to `ferrocast_core::show::LiveWatch`;
//! what it raises or clears goes to the dashboard (`live_alert`) and the log, and a black camera
//! can put the show on BRB by itself (`ScenesConfig::brb_when_black`).

use std::sync::atomic::Ordering;
use std::time::Duration;

use ferrocast_core::show::audio_watch::{MicLevel, SceneLook};
use ferrocast_core::show::{LiveAlert, WatchSample};
use serde_json::Value;

use crate::Engine;
use crate::now;
use crate::studio_host::StudioAct;

/// How often the watch looks.
const EVERY: Duration = Duration::from_secs(2);
/// A gap longer than this between 2-second ticks means the system slept or froze: reset
/// pending silence/black/frozen durations so wake never fires false live alerts.
const WAKE_JUMP_MS: i64 = 10_000;
/// Rate limit for live missed-frame breakdown logs (ms).
const MISS_LOG_EVERY_MS: i64 = 10_000;
/// Rate limit for output falling-behind warnings (ms).
const OUTPUT_LAG_LOG_EVERY_MS: i64 = 5_000;
/// Upload check comparison threshold: warn when total stream bitrate exceeds 85% of measured upload.
const TIGHT_UPLOAD_PCT: u64 = 85;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OutputLagReport {
    pub cause: &'static str,
    pub queue_mb: f64,
    pub dropped_delta: u64,
    pub dropped_total: u64,
    pub sender_kbps: u32,
    pub bitrate_kbps: u32,
    pub measured_upload_kbps: Option<u32>,
    pub measured_by: Option<String>,
    pub upload_status: &'static str,
    pub auto_quality_pct: u8,
    pub slow_dests: String,
}

#[derive(Debug, Default)]
pub(crate) struct OutputLagWatch {
    prev_at_ms: Option<i64>,
    prev_sender_bytes: u64,
    prev_sender_dropped: u64,
    prev_dest_dropped: std::collections::HashMap<String, u64>,
    prev_dest_live: std::collections::HashMap<String, bool>,
    pending_dropped: u64,
    last_warn_ms: Option<i64>,
}

pub(crate) struct OutputLagTick<'a> {
    pub at_ms: i64,
    pub sender_active: bool,
    pub sender_reconnecting: bool,
    pub sender_bytes: u64,
    pub sender_dropped: u64,
    pub dests: &'a [ferrocast_core::auto_quality::Dest],
    pub paused: bool,
}

impl OutputLagWatch {
    /// Checks sender and lightweight Go live destination counters every tick without calling `golive_view`
    /// unless a stall, drop, or disconnect requires upload-check & bitrate comparison details.
    pub(crate) fn detect_trouble(&mut self, tick: &OutputLagTick<'_>) -> Option<(&'static str, u32, u64)> {
        let Some(prev_ms) = self.prev_at_ms else {
            self.prev_at_ms = Some(tick.at_ms);
            self.prev_sender_bytes = tick.sender_bytes;
            self.prev_sender_dropped = tick.sender_dropped;
            for d in tick.dests {
                self.prev_dest_dropped.insert(d.id.clone(), d.dropped);
                self.prev_dest_live.insert(d.id.clone(), d.live);
            }
            return None;
        };
        if tick.at_ms < prev_ms
            || tick.sender_bytes < self.prev_sender_bytes
            || tick.sender_dropped < self.prev_sender_dropped
        {
            *self = Self::default();
            return self.detect_trouble(tick);
        }
        let dt_ms = (tick.at_ms - prev_ms).max(1) as u64;
        let d_bytes = tick.sender_bytes - self.prev_sender_bytes;
        let d_sender_dropped = tick.sender_dropped - self.prev_sender_dropped;
        let sender_kbps = u32::try_from(d_bytes.saturating_mul(8) / dt_ms).unwrap_or(u32::MAX);

        let mut d_dest_dropped = 0u64;
        let mut hung_up = false;
        for d in tick.dests {
            let prev_d = self.prev_dest_dropped.get(&d.id).copied().unwrap_or(0);
            if d.dropped >= prev_d {
                d_dest_dropped += d.dropped - prev_d;
            }
            let was_live = self.prev_dest_live.get(&d.id).copied().unwrap_or(false);
            if was_live && !d.live {
                hung_up = true;
            }
            self.prev_dest_dropped.insert(d.id.clone(), d.dropped);
            self.prev_dest_live.insert(d.id.clone(), d.live);
        }
        let stalled = tick.sender_active && !tick.sender_reconnecting && self.prev_sender_bytes > 0 && d_bytes == 0;
        self.prev_at_ms = Some(tick.at_ms);
        self.prev_sender_bytes = tick.sender_bytes;
        self.prev_sender_dropped = tick.sender_dropped;

        let step_drops = d_sender_dropped.saturating_add(d_dest_dropped);
        self.pending_dropped = self.pending_dropped.saturating_add(step_drops);

        let cause = if stalled {
            Some("output stalled (0 bytes sent)")
        } else if d_sender_dropped > 0 {
            Some("sender dropping packets")
        } else if d_dest_dropped > 0 {
            Some("destination falling behind")
        } else if hung_up || tick.paused {
            Some("destination disconnected or paused")
        } else {
            None
        };
        let cause = cause?;
        if !self.last_warn_ms.is_none_or(|t| tick.at_ms - t >= OUTPUT_LAG_LOG_EVERY_MS) {
            return None;
        }
        let burst = std::mem::take(&mut self.pending_dropped);
        self.last_warn_ms = Some(tick.at_ms);
        Some((cause, sender_kbps, burst))
    }

    pub(crate) fn build_report(input: OutputLagInput) -> OutputLagReport {
        let dest_kbps_sum: u32 = input.dest_details.iter().map(|(_, _, k)| *k).fold(0u32, u32::saturating_add);
        let dropped_total =
            input.sender_dropped.saturating_add(input.dest_details.iter().map(|(_, d, _)| *d).sum::<u64>());
        let bitrate_kbps = input.hub_kbps.max(dest_kbps_sum).max(input.sender_kbps);
        let (measured_upload_kbps, measured_by, upload_status) = match input.upload {
            Some((up, by)) if up > 0 => {
                let status = if bitrate_kbps > up {
                    "exceeds_upload"
                } else if u64::from(bitrate_kbps).saturating_mul(100) >= u64::from(up).saturating_mul(TIGHT_UPLOAD_PCT)
                {
                    "tight_upload"
                } else {
                    "within_upload"
                };
                (Some(up), Some(by), status)
            }
            _ => (None, None, "no_upload_check"),
        };
        let slow_dests = if input.dest_details.is_empty() {
            "none".into()
        } else {
            input
                .dest_details
                .into_iter()
                .map(|(name, dropped, kbps)| format!("{name}:{kbps}kbps/drop={dropped}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        OutputLagReport {
            cause: input.cause,
            queue_mb: input.queue_mb,
            dropped_delta: input.dropped_delta,
            dropped_total,
            sender_kbps: input.sender_kbps,
            bitrate_kbps,
            measured_upload_kbps,
            measured_by,
            upload_status,
            auto_quality_pct: input.auto_quality_pct,
            slow_dests,
        }
    }
}

pub(crate) struct OutputLagInput {
    pub cause: &'static str,
    pub queue_mb: f64,
    pub dropped_delta: u64,
    pub sender_dropped: u64,
    pub sender_kbps: u32,
    pub hub_kbps: u32,
    pub upload: Option<(u32, String)>,
    pub auto_quality_pct: u8,
    pub dest_details: Vec<(String, u64, u32)>,
}

/// Cumulative counters observed at one live tick to split misses into render vs encode vs network.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MissSample {
    pub at_ms: i64,
    pub render_missed: u64,
    pub encode_held: u64,
    pub encode_skipped: u64,
    pub stream_dropped: u64,
    pub platform_dropped: u64,
}

impl MissSample {
    pub(crate) fn from_stats(at_ms: i64, v: &Value, platform_dropped: u64) -> Self {
        let (e, s) = (&v["engine"], &v["stream"]);
        let n = |x: &Value| x.as_u64().unwrap_or(0);
        let lagged = n(&e["lagged"]);
        let encode_held = n(&e["missed_encode"]);
        let render_missed = if e.get("missed_render").is_some() || e.get("missed_encode").is_some() {
            n(&e["missed_render"])
        } else {
            lagged.saturating_sub(encode_held)
        };
        Self {
            at_ms,
            render_missed,
            encode_held,
            encode_skipped: n(&e["skipped"]),
            stream_dropped: n(&s["dropped"]),
            platform_dropped,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MissSummary {
    pub why: String,
    pub render_missed: u64,
    pub encode_missed: u64,
    pub network_dropped: u64,
    pub stream_dropped: u64,
    pub platform_dropped: u64,
}

#[derive(Debug, Default)]
pub(crate) struct MissWatch {
    prev: Option<MissSample>,
    pending_render: u64,
    pending_encode: u64,
    pending_stream: u64,
    pending_platform: u64,
    last_log_ms: Option<i64>,
}

impl MissWatch {
    pub(crate) fn observe(&mut self, s: MissSample) -> Option<MissSummary> {
        let Some(prev) = self.prev else {
            self.prev = Some(s);
            return None;
        };
        // Counters going backwards mean a new engine/stream session started.
        if s.render_missed < prev.render_missed
            || s.encode_held < prev.encode_held
            || s.encode_skipped < prev.encode_skipped
            || s.stream_dropped < prev.stream_dropped
            || s.platform_dropped < prev.platform_dropped
        {
            *self = Self { prev: Some(s), ..Self::default() };
            return None;
        }
        let d_render = s.render_missed - prev.render_missed;
        let d_encode = (s.encode_held - prev.encode_held).saturating_add(s.encode_skipped - prev.encode_skipped);
        let d_stream = s.stream_dropped - prev.stream_dropped;
        let d_platform = s.platform_dropped - prev.platform_dropped;
        self.prev = Some(s);

        self.pending_render += d_render;
        self.pending_encode += d_encode;
        self.pending_stream += d_stream;
        self.pending_platform += d_platform;

        let network_dropped = self.pending_stream.saturating_add(self.pending_platform);
        let has_misses = self.pending_render > 0 || self.pending_encode > 0 || network_dropped > 0;
        if !has_misses || !self.last_log_ms.is_none_or(|t| s.at_ms - t >= MISS_LOG_EVERY_MS) {
            return None;
        }

        let mut causes = Vec::new();
        if self.pending_render > 0 {
            causes.push("render");
        }
        if self.pending_encode > 0 {
            causes.push("encode");
        }
        if network_dropped > 0 {
            causes.push("network");
        }
        let out = MissSummary {
            why: causes.join(" + "),
            render_missed: self.pending_render,
            encode_missed: self.pending_encode,
            network_dropped,
            stream_dropped: self.pending_stream,
            platform_dropped: self.pending_platform,
        };
        self.pending_render = 0;
        self.pending_encode = 0;
        self.pending_stream = 0;
        self.pending_platform = 0;
        self.last_log_ms = Some(s.at_ms);
        Some(out)
    }
}

/// Whether a mixer row is judged as a microphone: the one the owner said ("This is a mic"), else
/// by its kind (a mic input or a Mic/Aux channel that isn't a capture card; never the desktop's
/// or an app's sound). A row from an older engine without the field is judged by its kind.
fn is_mic(row: &Value) -> bool {
    if let Some(m) = row["mic"].as_bool() {
        return m;
    }
    let kind = row["kind"].as_str().unwrap_or("");
    let channel = row["channel"].as_u64();
    matches!(kind, "coreaudio_input_capture" | "wasapi_input_capture" | "pulse_input_capture" | "alsa_input_capture")
        || channel.is_some_and(|c| (3..=6).contains(&c))
}

/// Whether the watch judges this mixer row at all: the Show's own mics always, else the
/// microphones, and in both cases only one that viewers can hear: on air (its item and the scenes
/// it is in shown; a hidden one is not mixed, so it reads as silence and is no fault), on the
/// stream's track (1), and not monitor-only.
fn judged(row: &Value, listed: &[String]) -> bool {
    let name = row["name"].as_str().unwrap_or("");
    if !(listed.iter().any(|l| l == name) || is_mic(row)) {
        return false;
    }
    if row["shown"].as_bool() == Some(false) || row["monitor"].as_str() == Some("only_me") {
        return false;
    }
    row["tracks"].as_array().is_none_or(|t| t.iter().any(|v| v.as_u64() == Some(1)))
}

/// The names the watch reads levels for.
pub(crate) fn judged_names(mixer: &Value, listed: &[String]) -> Vec<String> {
    mixer
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| judged(r, listed))
        .filter_map(|r| r["name"].as_str().map(String::from))
        .collect()
}

/// One look at the Studio for a tick: its scenes, the on-air scene's sounds, and the items of
/// that scene and the scenes shown inside it (by name).
#[derive(Debug, Default, Clone)]
pub(crate) struct WatchSnap {
    pub scenes: Option<Value>,
    pub audio: Option<Value>,
    pub items: serde_json::Map<String, Value>,
}

/// How many reads make one tick's level, and how far apart: each read covers the last ~300 ms,
/// so six reads 300 ms apart cover 1.8 s of the 2 s between ticks (one read covered 0.3).
const HOLD_READS: usize = 6;
const HOLD_GAP: Duration = if cfg!(test) { Duration::from_millis(1) } else { Duration::from_millis(300) };

/// Several reads of the same sounds' levels as one: each peak/magnitude the highest seen (the
/// sound since the last tick, not the one 300 ms before it), `gated` if it was at any read,
/// `speech` when at least 2 reads reported speech (rejects single-read transient spikes), the rest
/// as last read.
pub(crate) fn peak_hold(reads: &[Value]) -> Value {
    let mut out = reads.last().cloned().unwrap_or(Value::Null);
    let Some(obj) = out.as_object_mut() else { return out };
    for (name, level) in obj.iter_mut() {
        for key in ["peak", "magnitude", "input_peak"] {
            let best = reads
                .iter()
                .filter_map(|r| r[name.as_str()][key].as_array())
                .map(|a| a.iter().filter_map(Value::as_f64).collect::<Vec<f64>>())
                .fold(Vec::<f64>::new(), |acc, v| {
                    if acc.is_empty() {
                        v
                    } else {
                        acc.iter().zip(v.iter().chain(std::iter::repeat(&f64::MIN))).map(|(a, b)| a.max(*b)).collect()
                    }
                });
            if !best.is_empty() {
                level[key] = Value::from(best);
            }
        }
        let gated = reads.iter().any(|r| r[name.as_str()]["gated"].as_bool() == Some(true));
        level["gated"] = Value::Bool(gated);
        let speech_reads = reads.iter().filter(|r| r[name.as_str()]["speech"].as_bool() == Some(true)).count();
        level["speech"] = Value::Bool(speech_reads >= 2);
    }
    out
}

/// The loudest channel (dBFS) of a `[l, r]` level, None when absent.
fn loudest(v: &Value) -> Option<f32> {
    let m = v.as_array()?.iter().filter_map(Value::as_f64).fold(f64::MIN, f64::max);
    (m > f64::MIN).then_some(m as f32)
}

/// Every mic of a scene's mixer with its levels now. `listed` are the Show's own watched mics
/// (they keep their own "no sound" notice).
pub(crate) fn mic_levels(mixer: &Value, levels: &Value, listed: &[String]) -> Vec<MicLevel> {
    let mut out = Vec::new();
    for row in mixer.as_array().into_iter().flatten().filter(|r| judged(r, listed)) {
        let Some(name) = row["name"].as_str() else { continue };
        // A mic nobody has measured yet (just added) says nothing.
        let Some(peak) = loudest(&levels[name]["peak"]) else { continue };
        let rms = loudest(&levels[name]["magnitude"]).unwrap_or(peak);
        let input = loudest(&levels[name]["input_peak"]).unwrap_or(peak);
        out.push(MicLevel {
            name: name.to_string(),
            peak_db: peak,
            rms_db: rms,
            input_db: input,
            muted: row["muted"].as_bool() == Some(true),
            gated: levels[name]["gated"].as_bool() == Some(true),
            speech: levels[name]["speech"].as_bool().unwrap_or(false),
            volume_db: row["volume_db"].as_f64().unwrap_or(0.0) as f32,
            listed: listed.iter().any(|l| l == name),
        });
    }
    out
}

impl Engine {
    /// Starts the watch over (a new stream, a wake from sleep): every alert it had raised is told
    /// cleared (the page would show it for ever otherwise), and a BRB that a black camera put on
    /// by itself is taken off (with the alert gone nothing would ever do it). Returns the frames.
    pub(crate) async fn reset_live_watch(&self) -> Vec<LiveAlert> {
        let told = self.studio().live_watch.lock().unwrap_or_else(|p| p.into_inner()).reset_telling();
        for a in &told {
            self.studio_frame("live_alert", a);
        }
        let was_black = {
            let mut black = self.studio().auto_brb.lock().unwrap_or_else(|p| p.into_inner());
            let was = !black.is_empty();
            black.clear();
            was
        };
        if was_black
            && self.scenes_config().brb_when_black
            && let Err(e) = self.set_brb_for(super::BrbHold::BlackCam, false, &[]).await
        {
            tracing::warn!(error = %e, "live watch: couldn't take BRB off after the reset");
        }
        told
    }

    /// The streamer says an alert is on purpose (a muted mic): cleared now, not raised again
    /// for this stream.
    pub fn live_alert_silence(&self, id: &str) -> bool {
        if !id.starts_with("mic:") {
            return false;
        }
        let cleared = self.studio().live_watch.lock().unwrap_or_else(|p| p.into_inner()).silence(id);
        if let Some(a) = cleared {
            self.studio_frame("live_alert", &a);
        }
        true
    }

    /// Starts the live watch's loop (once per engine). It looks only while the Studio streams.
    pub fn watch_live(&self) {
        if self.studio().live_watching.swap(true, Ordering::SeqCst) {
            return;
        }
        let engine = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(EVERY);
            let mut was_live = false;
            loop {
                tick.tick().await;
                // One read of the Studio's own sender for the whole tick (streaming? dropped?).
                let sender = engine.studio_sender_stats().await;
                let live = sender.as_ref().is_some_and(|v| v["active"].as_bool() == Some(true));
                if live != was_live {
                    // A new stream: everything starts fresh (and the page is told what that dropped).
                    engine.reset_live_watch().await;
                    *engine.studio().last_watch_mono.lock().unwrap_or_else(|p| p.into_inner()) = None;
                    *engine.studio().scene_now.lock().unwrap_or_else(|p| p.into_inner()) = None;
                    *engine.studio().auto_quality.lock().unwrap_or_else(|p| p.into_inner()) = Default::default();
                    *engine.studio().live_health.lock().unwrap_or_else(|p| p.into_inner()) = Default::default();
                    *engine.studio().miss_watch.lock().unwrap_or_else(|p| p.into_inner()) = Default::default();
                    *engine.studio().output_lag_watch.lock().unwrap_or_else(|p| p.into_inner()) = Default::default();
                    if live {
                        engine.wake_youtube();
                    }
                    engine.wake_viewer_poller();
                    was_live = live;
                }
                // A recording stopping or running out of room matters live or not.
                engine.record_watch_tick().await;
                if live {
                    // One look at the Studio's scenes, sounds and items for everything below.
                    let snap = engine.watch_snapshot().await;
                    engine.scene_chapter_tick_with(snap.scenes.as_ref()).await;
                    let dropped = sender.as_ref().and_then(|v| v["dropped"].as_u64()).unwrap_or(0);
                    engine.auto_quality_tick_with(dropped).await;
                    engine.health_tick().await;
                    engine.output_lag_tick(sender.as_ref()).await;
                    engine.live_watch_tick_with(&snap).await;
                }
            }
        });
    }

    /// The Studio's own sender's stats, once a tick.
    async fn studio_sender_stats(&self) -> Option<Value> {
        self.studio_host().ok()?.stream_stats().await.ok()
    }

    /// The scenes, the on-air scene's sounds and the items needed to judge it, in one read on the
    /// Studio's thread (a tick used to make a handful of separate ones).
    pub(crate) async fn watch_snapshot(&self) -> WatchSnap {
        match self.studio_act(StudioAct::WatchSnapshot).await {
            Ok(v) => WatchSnap {
                scenes: Some(v["scenes"].clone()).filter(|s| !s.is_null()),
                audio: Some(v["audio"].clone()).filter(|a| !a.is_null()),
                items: v["items"].as_object().cloned().unwrap_or_default(),
            },
            Err(_) => WatchSnap::default(),
        }
    }

    /// The scene on air, when it changed: a chapter marker (the first one starts the stream's).
    #[cfg(test)]
    pub(crate) async fn scene_chapter_tick(&self) {
        if !self.moments_config().markers {
            return;
        }
        let Ok(v) = self.studio_act(StudioAct::Scenes).await else { return };
        self.scene_chapter_tick_with(Some(&v)).await;
    }

    /// [`Self::scene_chapter_tick`] on scenes already read (the tick's snapshot).
    pub(crate) async fn scene_chapter_tick_with(&self, scenes: Option<&Value>) {
        if !self.moments_config().markers {
            return;
        }
        let Some(v) = scenes else { return };
        let Some(scene) = v["current"].as_str().filter(|s| !s.is_empty()).map(String::from) else { return };
        {
            let mut now = self.studio().scene_now.lock().unwrap_or_else(|p| p.into_inner());
            if now.as_deref() == Some(scene.as_str()) {
                return;
            }
            *now = Some(scene.clone());
        }
        if let Err(e) = self.add_marker_now(ferrocast_core::moments::MomentKind::Scene, &scene, None, false).await {
            tracing::warn!(error = %e, "couldn't mark a scene change");
        }
    }

    /// Auto quality: trouble getting the stream out lowers its bitrate a step, calm raises it
    /// back (when Go live's switch is on).
    /// Auto quality's look at the sender's skipped packets (read once a tick).
    pub(crate) async fn auto_quality_tick_with(&self, feed_dropped: u64) {
        let Ok(host) = self.studio_host() else { return };
        let (_, paused) = self.golive_trouble();
        let senders = self.golive_senders();
        // Live with nothing sending to a platform (kept on for a recording): no upload to judge,
        // so the bitrate isn't stepped down or up for it.
        if senders.is_empty() && self.golive_stream_running() {
            return;
        }
        let s = ferrocast_core::auto_quality::Signals { dests: senders, feed_dropped, paused };
        let changed = {
            let mut aq = self.studio().auto_quality.lock().unwrap_or_else(|p| p.into_inner());
            // Switched off, it still watches: the status bar says when the upload struggles.
            if !self.golive_config().auto_quality {
                aq.observe(now().0, s);
                return;
            }
            aq.tick(now().0, s)
        };
        let Some(pct) = changed else { return };
        tracing::info!(pct, "auto quality changed the stream's bitrate share");
        if let Err(e) = host.set_live_quality(pct).await {
            tracing::warn!(error = %e, "auto quality couldn't change the bitrate");
            return;
        }
        let alert = if pct >= 100 {
            LiveAlert::fixed("auto_quality", "warn", "Your stream is back to full quality.")
        } else {
            LiveAlert::raised("auto_quality", "warn", format!("Upload struggled: quality is at {pct}%"))
                .detail("Ferrocast lowered the quality to keep the stream smooth. It goes back up by itself.")
        };
        self.studio_frame("live_alert", &alert);
    }

    /// The stream's health: the engine's and the network's counters into the score.
    pub(crate) async fn health_tick(&self) {
        let Ok(host) = self.studio_host() else { return };
        let Ok(v) = host.stats().await else { return };
        self.observe_sidechain_cycles(&v);
        let (e, s) = (&v["engine"], &v["stream"]);
        let n = |x: &Value| x.as_u64().unwrap_or(0);
        let (net_dropped, paused) = self.golive_trouble();
        let sample = ferrocast_core::health::Sample {
            frames: n(&e["frames"]),
            lagged: n(&e["lagged"]),
            encoded: n(&e["encoded"]),
            skipped: n(&e["skipped"]),
            sent: n(&s["frames"]),
            sent_bytes: n(&s["bytes"]),
            dropped: n(&s["dropped"]),
            net_dropped,
            reconnecting: s["reconnecting"].as_bool() == Some(true) || self.golive_main_reconnecting().is_some(),
            paused,
            quality_pct: self.studio().auto_quality.lock().unwrap_or_else(|p| p.into_inner()).pct(),
            cpu: e["cpu"].as_f64().unwrap_or(0.0),
            encoding_ms: n(&e["encoding_ms"]),
            at_ms: u64::try_from(now().0).unwrap_or(0),
        };
        let mut h = self.studio().live_health.lock().unwrap_or_else(|p| p.into_inner());
        let health_now = h.0.push(sample);
        h.1 = Some(health_now);
        drop(h);
        let miss_sample = MissSample::from_stats(now().0, &v, net_dropped);
        let miss_summary = self.studio().miss_watch.lock().unwrap_or_else(|p| p.into_inner()).observe(miss_sample);
        if let Some(m) = miss_summary {
            tracing::warn!(
                why = %m.why,
                render_missed = m.render_missed,
                encode_missed = m.encode_missed,
                network_dropped = m.network_dropped,
                stream_dropped = m.stream_dropped,
                platform_dropped = m.platform_dropped,
                "missed or dropped frames during stream ({})",
                m.why
            );
        }
    }

    /// Diagnoses output freezes and falling-behind destinations with queue depth, dropped counts,
    /// and bitrate vs measured upload capacity (rate-limited at warn).
    pub(crate) async fn output_lag_tick(&self, sender: Option<&Value>) {
        let Some(s) = sender else { return };
        let active = s["active"].as_bool() == Some(true);
        let reconnecting = s["reconnecting"].as_bool() == Some(true);
        let sender_bytes = s["bytes"].as_u64().unwrap_or(0);
        let sender_dropped = s["dropped"].as_u64().unwrap_or(0);
        let queue_mb = s["delay_mb"].as_f64().unwrap_or(0.0);
        let (_, paused) = self.golive_trouble();
        let dests = self.golive_senders();

        let hit =
            self.studio().output_lag_watch.lock().unwrap_or_else(|p| p.into_inner()).detect_trouble(&OutputLagTick {
                at_ms: now().0,
                sender_active: active,
                sender_reconnecting: reconnecting,
                sender_bytes,
                sender_dropped,
                dests: &dests,
                paused,
            });
        let Some((cause, sender_kbps, dropped_delta)) = hit else { return };

        let gl = self.golive_view().await;
        let upload = gl.upload.as_ref().map(|u| (u.verdict.upload_kbps, u.measured_by.clone()));
        let dest_details = gl
            .destinations
            .iter()
            .filter_map(|d| d.health.as_ref().map(|h| (d.name.clone(), h.dropped, h.kbps)))
            .collect();
        let aq_pct = self.studio().auto_quality.lock().unwrap_or_else(|p| p.into_inner()).pct();
        let r = OutputLagWatch::build_report(OutputLagInput {
            cause,
            queue_mb,
            dropped_delta,
            sender_dropped,
            sender_kbps,
            hub_kbps: gl.stats.kbps,
            upload,
            auto_quality_pct: aq_pct,
            dest_details,
        });
        tracing::warn!(
            cause = r.cause,
            queue_mb = r.queue_mb,
            dropped_delta = r.dropped_delta,
            dropped_total = r.dropped_total,
            sender_kbps = r.sender_kbps,
            bitrate_kbps = r.bitrate_kbps,
            measured_upload_kbps = r.measured_upload_kbps,
            measured_by = r.measured_by.as_deref(),
            upload_status = r.upload_status,
            auto_quality_pct = r.auto_quality_pct,
            destinations = %r.slow_dests,
            "output falling behind: {}",
            r.cause
        );
    }

    /// Auto quality turned off: a lowered stream goes back to full quality.
    pub(crate) async fn auto_quality_off(&self) {
        let lowered = {
            let mut aq = self.studio().auto_quality.lock().unwrap_or_else(|p| p.into_inner());
            aq.back_to_full()
        };
        if !lowered {
            return;
        }
        if let Ok(host) = self.studio_host()
            && let Err(e) = host.set_live_quality(100).await
        {
            tracing::warn!(error = %e, "auto quality couldn't put the bitrate back");
        }
        self.studio_frame(
            "live_alert",
            &LiveAlert::fixed("auto_quality", "warn", "Your stream is back to full quality."),
        );
    }

    /// The levels of these sounds over the last ~1.8 s: [`HOLD_READS`] wait-free reads 300 ms
    /// apart, the highest peak of each (the 300 ms window of one read left most of a tick unseen).
    async fn held_levels(&self, names: Vec<String>) -> Result<Value, crate::EngineError> {
        let mut reads = Vec::with_capacity(HOLD_READS);
        for i in 0..HOLD_READS {
            if i > 0 {
                tokio::time::sleep(HOLD_GAP).await;
            }
            reads.push(self.studio_levels(names.clone(), false).await?);
        }
        Ok(peak_hold(&reads))
    }

    /// What goes out now: every mic on air with its levels, the scene on air, the Show's cameras
    /// and Go live's destinations.
    #[cfg(test)]
    pub(crate) async fn watch_sample(&self, mono: i64, show: &ferrocast_core::show::ShowConfig) -> WatchSample {
        let snap = self.watch_snapshot().await;
        self.watch_sample_with(mono, show, &snap).await
    }

    /// [`Self::watch_sample`] on one snapshot of the Studio: with it a tick asks the Studio's
    /// thread for at most two things (the snapshot, and the cameras' look).
    pub(crate) async fn watch_sample_with(
        &self,
        mono: i64,
        show: &ferrocast_core::show::ShowConfig,
        snap: &WatchSnap,
    ) -> WatchSample {
        let mut sample = WatchSample { at: mono, ..Default::default() };
        // Every mic on air (not only the Show's list), and the scene they're in.
        let scenes = snap.scenes.clone();
        let on_air = scenes.as_ref().and_then(|v| v["current"].as_str()).filter(|n| !n.is_empty()).map(String::from);
        if let Some(scene) = &on_air {
            // The Show's own starting, ending and BRB scenes: a mic is muted on purpose there.
            let brb = self.scenes_config().brb_scene;
            sample.scene_exempt = [show.starting_scene.as_deref(), show.ending_scene.as_deref(), brb.as_deref()]
                .into_iter()
                .flatten()
                .any(|n| n == scene);
            if let Some(mixer) = snap.audio.clone() {
                let names = judged_names(&mixer, &show.mic_sources);
                if !names.is_empty()
                    && let Ok(v) = self.held_levels(names).await
                {
                    sample.levels = mic_levels(&mixer, &v, &show.mic_sources);
                    // The Show's own list reads the same sound (no second, UI-thread, read).
                    for m in &show.mic_sources {
                        let peak = v[m]["peak"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_f64)
                            .fold(f64::MIN, f64::max);
                        if peak > f64::MIN {
                            sample.mics.push((m.clone(), peak as f32));
                        }
                    }
                }
            }
            if let Some(empty) = crate::studio::rig::studio_scene_is_empty_in(&snap.items, scene) {
                let faded = scenes.as_ref().is_some_and(|v| v["black"].as_bool() == Some(true));
                sample.scene = Some(SceneLook { name: scene.clone(), empty, faded });
            }
        }
        if !show.cam_sources.is_empty()
            && let Ok(v) = self.studio_act(StudioAct::Look { names: show.cam_sources.clone() }).await
        {
            for c in &show.cam_sources {
                let (Some(luma), Some(spread), Some(sig)) =
                    (v[c]["luma"].as_u64(), v[c]["spread"].as_u64(), v[c]["sig"].as_u64())
                else {
                    continue;
                };
                sample.cameras.push((c.clone(), luma < 12 && spread < 6, sig));
            }
        }
        sample.destinations = self.golive_sending();
        sample
    }

    /// Before Go live: a scene that will be on air with no picture, and a mic (not on the Show's
    /// own list, which has its checks) that is muted or sends nothing. Warnings: they never block.
    pub(crate) async fn preflight_picture_and_mics(
        &self,
        rig: &crate::studio::rig::Rig,
        cfg: &ferrocast_core::show::ShowConfig,
        on_air: Option<&str>,
        checks: &mut Vec<ferrocast_core::show::Check>,
    ) {
        use ferrocast_core::show::{Check, CheckStatus, fix};
        if !matches!(rig, crate::studio::rig::Rig::Studio) {
            return;
        }
        // The scenes viewers will see: the starting one (when there is one), then the live one,
        // or whatever is on air now.
        let mut scenes: Vec<&str> = Vec::new();
        for s in [cfg.starting_scene.as_deref(), cfg.live_scene.as_deref().or(on_air)].into_iter().flatten() {
            if !scenes.contains(&s) {
                scenes.push(s);
            }
        }
        for s in &scenes {
            if self.rig_scene_is_empty(rig, s).await == Some(true) {
                checks.push(Check::new(
                    &format!("picture:{s}"),
                    "Picture",
                    CheckStatus::Warn,
                    format!("\"{s}\" has no camera, screen or image: viewers will see black."),
                    fix("Pick another scene", "#/studio/show"),
                ));
            }
        }
        let Some(scene) = cfg.live_scene.as_deref().or(on_air) else { return };
        let Ok(mixer) = self.studio_act(StudioAct::Audio { scene: scene.into() }).await else { return };
        let names: Vec<String> =
            judged_names(&mixer, &cfg.mic_sources).into_iter().filter(|n| !cfg.mic_sources.contains(n)).collect();
        if names.is_empty() {
            return;
        }
        let levels = self.held_levels(names).await.unwrap_or(Value::Null);
        let mics = mic_levels(&mixer, &levels, &cfg.mic_sources);
        // A mic carrying the voice: unmuted, not held shut, not dead. Another one muted on purpose
        // is no warning then (the live watch says the same).
        let carries = |me: &str| {
            mics.iter().any(|m| {
                m.name != me
                    && !m.muted
                    && m.volume_db > ferrocast_core::show::audio_watch::FADER_DOWN_DB
                    && !m.gated
                    && m.input_db > ferrocast_core::show::audio_watch::DEAD_DB
            })
        };
        for m in &mics {
            let id = format!("mic:{}", m.name);
            let off = m.muted || m.volume_db <= ferrocast_core::show::audio_watch::FADER_DOWN_DB;
            if off && !m.gated {
                if carries(&m.name) {
                    continue;
                }
                checks.push(Check::new(
                    &id,
                    &m.name,
                    CheckStatus::Warn,
                    if m.muted {
                        "Muted: nobody will hear you."
                    } else {
                        "Turned all the way down: nobody will hear you."
                    },
                    fix("Unmute", format!("action:unmute:{}", m.name)),
                ));
            } else if !off && !m.gated && m.input_db <= ferrocast_core::show::audio_watch::DEAD_DB && !carries(&m.name)
            {
                checks.push(Check::new(
                    &id,
                    &m.name,
                    CheckStatus::Warn,
                    "Not muted, but it sends nothing: check the cable and the input.",
                    fix("Check the mic", "#/studio/engine"),
                ));
            }
        }
    }

    /// One look → the alerts it raises or clears, sent on.
    #[cfg(test)]
    pub(crate) async fn live_watch_tick(&self) -> Vec<LiveAlert> {
        let snap = self.watch_snapshot().await;
        self.live_watch_tick_with(&snap).await
    }

    /// [`Self::live_watch_tick`] on one snapshot of the Studio.
    pub(crate) async fn live_watch_tick_with(&self, snap: &WatchSnap) -> Vec<LiveAlert> {
        let cfg = self.show_config().watch;
        if !cfg.on {
            return Vec::new();
        }
        let show = self.show_config();
        let mono = self.mono_ms();
        let wake_jumped = {
            let mut last = self.studio().last_watch_mono.lock().unwrap_or_else(|p| p.into_inner());
            let jumped = last.is_some_and(|prev| mono - prev > WAKE_JUMP_MS);
            *last = Some(mono);
            jumped
        };
        if wake_jumped {
            self.reset_live_watch().await;
        }
        let sample = self.watch_sample_with(mono, &show, snap).await;
        let alerts = self.studio().live_watch.lock().unwrap_or_else(|p| p.into_inner()).tick(&cfg, &sample);
        for a in &alerts {
            tracing::info!(id = %a.id, cleared = a.cleared, "live watch: {}", a.message);
            self.studio_frame("live_alert", a);
        }
        self.brb_when_black(&alerts).await;
        alerts
    }

    /// BRB by itself while a watched camera is black (when asked), back once none is.
    pub(crate) async fn brb_when_black(&self, alerts: &[LiveAlert]) {
        if !self.scenes_config().brb_when_black {
            return;
        }
        let (was, now) = {
            let mut black = self.studio().auto_brb.lock().unwrap_or_else(|p| p.into_inner());
            let was = !black.is_empty();
            for a in alerts.iter().filter(|a| a.id.ends_with(":black")) {
                if a.cleared {
                    black.remove(&a.id);
                } else {
                    black.insert(a.id.clone());
                }
            }
            (was, !black.is_empty())
        };
        if was != now {
            tracing::info!(on = now, "live watch: BRB by itself (a camera went black, or came back)");
            if let Err(e) = self.set_brb_for(super::BrbHold::BlackCam, now, &[]).await {
                tracing::warn!(error = %e, "live watch: couldn't switch BRB");
            }
        }
    }

    /// The live watch's alerts raised now (their ids).
    pub fn live_alerts(&self) -> Vec<String> {
        self.studio().live_watch.lock().unwrap_or_else(|p| p.into_inner()).raised()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(name: &str, extra: Value) -> Value {
        let mut r = json!({ "name": name, "kind": "coreaudio_input_capture", "tracks": [1], "monitor": "off", "shown": true, "mic": true });
        if let (Some(o), Some(e)) = (r.as_object_mut(), extra.as_object()) {
            o.extend(e.clone());
        }
        r
    }

    /// Only microphones viewers can hear are judged: the Show's own list always; a capture card,
    /// the desktop, a hidden item, a monitor-only mic and one off the stream's track never.
    #[test]
    fn only_mics_viewers_can_hear_are_judged() {
        let mixer = json!([
            row("KREO", json!({})),
            row("USB2 Digital Audio", json!({ "mic": false })),
            row("Desktop", json!({ "mic": false, "kind": "coreaudio_output_capture", "channel": 1 })),
            row("Hidden", json!({ "shown": false })),
            row("Monitor", json!({ "monitor": "only_me" })),
            row("Track2", json!({ "tracks": [2] })),
            row("Both", json!({ "tracks": [1, 2], "monitor": "me_and_stream" })),
        ]);
        assert_eq!(judged_names(&mixer, &[]), ["KREO", "Both"]);
        // The Show's own list overrides the owner's switch (not what viewers can hear, though).
        assert_eq!(
            judged_names(&mixer, &["USB2 Digital Audio".into(), "Hidden".into()]),
            ["KREO", "USB2 Digital Audio", "Both"]
        );
        // An older engine's row without the new fields is judged by its kind.
        let old = json!([{ "name": "Mic", "kind": "coreaudio_input_capture" }, { "name": "Spot", "kind": "sck_audio_capture" }]);
        assert_eq!(judged_names(&old, &[]), ["Mic"]);
    }

    /// One tick's level is the highest of its reads (the sound since the last tick), a key held
    /// at any read counts, and a sound missing from a read doesn't break the rest.
    #[test]
    fn a_ticks_level_is_the_highest_of_its_reads() {
        let read = |peak: f64, input: f64, gated: bool| json!({ "KREO": { "peak": [peak, peak - 1.0], "input_peak": [input, input], "gated": gated } });
        let held = peak_hold(&[read(-50.0, -50.0, false), read(-12.0, -14.0, false), read(-40.0, -40.0, true)]);
        assert_eq!(held["KREO"]["peak"], json!([-12.0, -13.0]));
        assert_eq!(held["KREO"]["input_peak"], json!([-14.0, -14.0]));
        assert_eq!(held["KREO"]["gated"], json!(true));
        let one = peak_hold(&[read(-30.0, -30.0, false)]);
        assert_eq!(one["KREO"]["peak"], json!([-30.0, -31.0]));
        assert_eq!(one["KREO"]["gated"], json!(false));
        assert_eq!(peak_hold(&[]), Value::Null);
    }

    #[test]
    fn peak_hold_aggregates_magnitude_and_requires_two_speech_reads() {
        let read = |peak: f64, mag: f64, speech: bool| json!({ "KREO": { "peak": [peak, peak], "magnitude": [mag, mag], "input_peak": [peak, peak], "gated": false, "speech": speech } });
        // Only 1 speech read out of 6 -> speech stays false (single-spike veto).
        let one_spike = peak_hold(&[
            read(-55.0, -60.0, false),
            read(-30.0, -40.0, true),
            read(-55.0, -60.0, false),
            read(-55.0, -60.0, false),
            read(-55.0, -60.0, false),
            read(-55.0, -60.0, false),
        ]);
        assert_eq!(one_spike["KREO"]["speech"], json!(false));
        assert_eq!(one_spike["KREO"]["magnitude"], json!([-40.0, -40.0]));

        // 2 speech reads out of 6 -> speech is true even if the trailing read was a pause.
        let two_reads = peak_hold(&[
            read(-38.0, -45.0, true),
            read(-36.0, -43.0, true),
            read(-62.0, -66.0, false),
            read(-62.0, -66.0, false),
            read(-62.0, -66.0, false),
            read(-62.0, -66.0, false),
        ]);
        assert_eq!(two_reads["KREO"]["speech"], json!(true));
        assert_eq!(two_reads["KREO"]["peak"], json!([-36.0, -36.0]));
        assert_eq!(two_reads["KREO"]["magnitude"], json!([-43.0, -43.0]));

        let mixer = json!([row("KREO", json!({}))]);
        let mics = mic_levels(&mixer, &two_reads, &[]);
        assert_eq!(mics.len(), 1);
        assert!(mics[0].speech);
        assert_eq!(mics[0].rms_db, -43.0);

        // Missing speech field (older engine) safely defaults to false.
        let legacy = json!({ "KREO": { "peak": [-40.0, -40.0], "input_peak": [-40.0, -40.0] } });
        let legacy_mics = mic_levels(&mixer, &legacy, &[]);
        assert!(!legacy_mics[0].speech);
    }

    #[test]
    fn miss_watch_separates_render_encode_network_and_rate_limits() {
        let mut w = MissWatch::default();
        let sample = |at_ms, r, eh, es, sd, pd| MissSample {
            at_ms,
            render_missed: r,
            encode_held: eh,
            encode_skipped: es,
            stream_dropped: sd,
            platform_dropped: pd,
        };
        assert_eq!(w.observe(sample(1_000, 0, 0, 0, 0, 0)), None);
        let first = w.observe(sample(3_000, 4, 2, 1, 5, 3)).unwrap_or_else(|| panic!("first miss summary"));
        assert_eq!(first.why, "render + encode + network");
        assert_eq!((first.render_missed, first.encode_missed, first.network_dropped), (4, 3, 8));
        assert_eq!((first.stream_dropped, first.platform_dropped), (5, 3));

        // Within rate-limit window (< 10 s): accumulates without logging.
        assert_eq!(w.observe(sample(5_000, 4, 2, 1, 5, 7)), None);
        // Past 10 s: flushes accumulated network drops cleanly.
        let second = w.observe(sample(13_000, 4, 2, 1, 5, 9)).unwrap_or_else(|| panic!("rate-limited flush"));
        assert_eq!(second.why, "network");
        assert_eq!((second.render_missed, second.encode_missed, second.network_dropped), (0, 0, 6));
    }

    #[test]
    fn output_lag_watch_detects_stalls_drops_rate_limits_and_compares_upload() {
        let mut w = OutputLagWatch::default();
        let dest = |dropped, live| {
            vec![ferrocast_core::auto_quality::Dest { id: "twitch".into(), dropped, live_ms: 30_000, live }]
        };

        // Warm baseline emits nothing.
        assert_eq!(
            w.detect_trouble(&OutputLagTick {
                at_ms: 1_000,
                sender_active: true,
                sender_reconnecting: false,
                sender_bytes: 100_000,
                sender_dropped: 0,
                dests: &dest(0, true),
                paused: false,
            }),
            None
        );

        // 2 s later with 0 additional bytes sent: 1-2 s output freeze triggers at once.
        let (cause, kbps, _) = w
            .detect_trouble(&OutputLagTick {
                at_ms: 3_000,
                sender_active: true,
                sender_reconnecting: false,
                sender_bytes: 100_000,
                sender_dropped: 0,
                dests: &dest(0, true),
                paused: false,
            })
            .unwrap_or_else(|| panic!("freeze detected"));
        assert_eq!(cause, "output stalled (0 bytes sent)");
        assert_eq!(kbps, 0);

        // Drops within the 5 s cooldown accumulate silently without re-warning yet.
        assert_eq!(
            w.detect_trouble(&OutputLagTick {
                at_ms: 5_000,
                sender_active: true,
                sender_reconnecting: false,
                sender_bytes: 200_000,
                sender_dropped: 3,
                dests: &dest(4, true),
                paused: false,
            }),
            None
        );

        // Past the 5 s cooldown, accumulated drops flush together with measured bitrate vs upload.
        let (cause2, _, burst) = w
            .detect_trouble(&OutputLagTick {
                at_ms: 9_000,
                sender_active: true,
                sender_reconnecting: false,
                sender_bytes: 1_200_000,
                sender_dropped: 5,
                dests: &dest(7, true),
                paused: false,
            })
            .unwrap_or_else(|| panic!("drop summary after cooldown"));
        assert_eq!(cause2, "sender dropping packets");
        assert_eq!(burst, 12);

        let over = OutputLagWatch::build_report(OutputLagInput {
            cause: cause2,
            queue_mb: 2.5,
            dropped_delta: burst,
            sender_dropped: 5,
            sender_kbps: 2000,
            hub_kbps: 7500,
            upload: Some((6000, "twitch".into())),
            auto_quality_pct: 85,
            dest_details: vec![("Twitch".into(), 7, 7500)],
        });
        assert_eq!(over.upload_status, "exceeds_upload");
        assert_eq!(over.measured_upload_kbps, Some(6000));
        assert_eq!(over.queue_mb, 2.5);

        let missing = OutputLagWatch::build_report(OutputLagInput {
            cause,
            queue_mb: 0.0,
            dropped_delta: 0,
            sender_dropped: 0,
            sender_kbps: 0,
            hub_kbps: 6000,
            upload: None,
            auto_quality_pct: 100,
            dest_details: vec![],
        });
        assert_eq!(missing.upload_status, "no_upload_check");
    }
}
