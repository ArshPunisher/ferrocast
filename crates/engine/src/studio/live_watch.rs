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

/// Several reads of the same sounds' levels as one: each peak the highest seen (the sound since
/// the last tick, not the one 300 ms before it), `gated` if it was at any read, the rest as last read.
pub(crate) fn peak_hold(reads: &[Value]) -> Value {
    let mut out = reads.last().cloned().unwrap_or(Value::Null);
    let Some(obj) = out.as_object_mut() else { return out };
    for (name, level) in obj.iter_mut() {
        for key in ["peak", "input_peak"] {
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
        let input = loudest(&levels[name]["input_peak"]).unwrap_or(peak);
        out.push(MicLevel {
            name: name.to_string(),
            peak_db: peak,
            input_db: input,
            muted: row["muted"].as_bool() == Some(true),
            gated: levels[name]["gated"].as_bool() == Some(true),
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
                    engine.studio().group_auto_quality.lock().unwrap_or_else(|p| p.into_inner()).clear();
                    *engine.studio().live_health.lock().unwrap_or_else(|p| p.into_inner()) = Default::default();
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

    /// Auto quality: trouble getting the stream out lowers an encode group's bitrate a step,
    /// calm raises it back (when Go live's switch is on). Scoped per encode group so one struggling
    /// group never drags down healthy destinations on another group.
    pub(crate) async fn auto_quality_tick_with(&self, feed_dropped: u64) {
        self.auto_quality_tick_at(now().0, feed_dropped).await;
    }

    pub(crate) async fn auto_quality_tick_at(&self, ts: i64, feed_dropped: u64) {
        let Ok(host) = self.studio_host() else { return };
        let (_, paused) = self.golive_trouble();
        let grouped = self.golive_group_senders();
        let enabled = self.golive_config().auto_quality;

        if grouped.is_empty() {
            let senders = self.golive_senders();
            // Live with nothing sending to a platform (kept on for a recording): no upload to judge.
            if senders.is_empty() && self.golive_stream_running() {
                return;
            }
            let s = ferrocast_core::auto_quality::Signals { dests: senders, feed_dropped, paused };
            let changed = {
                let mut aq = self.studio().auto_quality.lock().unwrap_or_else(|p| p.into_inner());
                if !enabled {
                    aq.observe(ts, s);
                    return;
                }
                aq.tick(ts, s)
            };
            let Some(pct) = changed else { return };
            if self.ffenc_handover_session_exhausted() {
                tracing::warn!(pct, "auto quality skipped bitrate change: no spare FFmpeg/NVENC session left");
                return;
            }
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
            return;
        }

        let group_stats = host.group_stats().await.ok().flatten();
        let active_ids: std::collections::HashSet<String> = grouped.keys().cloned().collect();
        let mut group_changes: Vec<(String, u8, u8)> = Vec::new();
        {
            let mut map = self.studio().group_auto_quality.lock().unwrap_or_else(|p| p.into_inner());
            map.retain(|k, _| active_ids.contains(k));
            let mut agg_dests = Vec::new();
            for (gid, senders) in &grouped {
                let g_dropped = group_stats
                    .as_ref()
                    .and_then(|v| v.get(gid))
                    .and_then(|s| s.get("dropped"))
                    .and_then(Value::as_u64)
                    .unwrap_or(if gid == crate::golive::GROUP_MAIN { feed_dropped } else { 0 });
                let sig =
                    ferrocast_core::auto_quality::Signals { dests: senders.clone(), feed_dropped: g_dropped, paused };
                let entry = map.entry(gid.clone()).or_default();
                let prev_pct = entry.pct();
                if enabled {
                    if let Some(next_pct) = entry.tick(ts, sig) {
                        group_changes.push((gid.clone(), prev_pct, next_pct));
                    }
                } else {
                    entry.observe(ts, sig);
                }
                agg_dests.extend(senders.iter().cloned());
            }
            let min_pct = map.values().map(ferrocast_core::auto_quality::AutoQuality::pct).min().unwrap_or(100);
            let agg_sig = ferrocast_core::auto_quality::Signals { dests: agg_dests, feed_dropped, paused };
            let mut legacy = self.studio().auto_quality.lock().unwrap_or_else(|p| p.into_inner());
            legacy.observe(ts, agg_sig);
            let _ = min_pct;
        }

        for (gid, _prev_pct, pct) in group_changes {
            if self.ffenc_handover_session_exhausted() {
                // Revert the controller's internal step so it doesn't silently think bitrate changed.
                let mut map = self.studio().group_auto_quality.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(aq) = map.get_mut(&gid) {
                    let _ = aq.back_to_full();
                }
                tracing::warn!(group = %gid, pct, "auto quality skipped step: FFmpeg/NVENC session limit reached");
                continue;
            }
            tracing::info!(group = %gid, pct, "auto quality changed encode group bitrate share");
            if let Err(e) = host.set_group_live_quality(&gid, pct).await {
                tracing::warn!(group = %gid, error = %e, "auto quality couldn't change the group's bitrate");
                continue;
            }
            let alert = if pct >= 100 {
                LiveAlert::fixed("auto_quality", "warn", "Your stream is back to full quality.")
            } else {
                LiveAlert::raised("auto_quality", "warn", format!("Upload struggled: quality is at {pct}%"))
                    .detail("Ferrocast lowered the quality to keep the stream smooth. It goes back up by itself.")
            };
            self.studio_frame("live_alert", &alert);
        }
    }

    /// The stream's health: the engine's and the network's counters into the score.
    pub(crate) async fn health_tick(&self) {
        let Ok(host) = self.studio_host() else { return };
        let Ok(v) = host.stats().await else { return };
        self.observe_sidechain_cycles(&v);
        let (e, s) = (&v["engine"], &v["stream"]);
        let n = |x: &Value| x.as_u64().unwrap_or(0);
        let (net_dropped, paused) = self.golive_trouble();
        let quality_pct = {
            let group_min = self
                .studio()
                .group_auto_quality
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .values()
                .map(ferrocast_core::auto_quality::AutoQuality::pct)
                .min();
            group_min.unwrap_or_else(|| self.studio().auto_quality.lock().unwrap_or_else(|p| p.into_inner()).pct())
        };
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
            quality_pct,
            cpu: e["cpu"].as_f64().unwrap_or(0.0),
            encoding_ms: n(&e["encoding_ms"]),
            at_ms: u64::try_from(now().0).unwrap_or(0),
        };
        let mut h = self.studio().live_health.lock().unwrap_or_else(|p| p.into_inner());
        let now = h.0.push(sample);
        h.1 = Some(now);
    }

    /// Auto quality turned off: lowered streams go back to full quality.
    pub(crate) async fn auto_quality_off(&self) {
        let lowered_groups: Vec<String> = {
            let mut map = self.studio().group_auto_quality.lock().unwrap_or_else(|p| p.into_inner());
            map.iter_mut().filter_map(|(gid, aq)| aq.back_to_full().then(|| gid.clone())).collect()
        };
        let legacy_lowered = {
            let mut aq = self.studio().auto_quality.lock().unwrap_or_else(|p| p.into_inner());
            aq.back_to_full()
        };
        if lowered_groups.is_empty() && !legacy_lowered {
            return;
        }
        if let Ok(host) = self.studio_host() {
            for gid in &lowered_groups {
                if let Err(e) = host.set_group_live_quality(gid, 100).await {
                    tracing::warn!(group = %gid, error = %e, "auto quality couldn't put the group's bitrate back");
                }
            }
            if lowered_groups.is_empty()
                && legacy_lowered
                && let Err(e) = host.set_live_quality(100).await
            {
                tracing::warn!(error = %e, "auto quality couldn't put the bitrate back");
            }
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

    #[tokio::test]
    async fn per_group_auto_quality_throttles_only_struggling_group_and_guards_nvenc_sessions() {
        use crate::golive::{Canvas, GROUP_MAIN};
        use crate::studio_host::{HostBudgetInfo, StudioHost, StudioSettings, StudioStatus};
        use std::sync::{Arc, Mutex};

        #[derive(Debug)]
        struct GroupMockHost {
            budget: Mutex<Option<HostBudgetInfo>>,
            stats: Mutex<Value>,
            group_calls: Mutex<Vec<(String, u8)>>,
            legacy_calls: Mutex<Vec<u8>>,
        }

        #[async_trait::async_trait]
        impl StudioHost for GroupMockHost {
            async fn status(&self) -> StudioStatus {
                Default::default()
            }
            async fn start(&self) -> Result<StudioStatus, String> {
                Err("n/a".into())
            }
            async fn stop(&self) -> Result<StudioStatus, String> {
                Err("n/a".into())
            }
            async fn kinds(&self) -> Result<Value, String> {
                Err("n/a".into())
            }
            async fn options(&self, _kind: String) -> Result<Value, String> {
                Err("n/a".into())
            }
            async fn preview(&self, _rect: Option<crate::studio_host::PreviewRect>) -> Result<(), String> {
                Ok(())
            }
            async fn defaults(&self, _kind: String) -> Result<Value, String> {
                Err("n/a".into())
            }
            async fn act(&self, _act: crate::studio_host::StudioAct) -> Result<Value, String> {
                Err("n/a".into())
            }
            async fn settings(&self) -> StudioSettings {
                Default::default()
            }
            async fn set_settings(&self, _s: StudioSettings) -> Result<Option<String>, String> {
                Ok(None)
            }
            async fn stats(&self) -> Result<Value, String> {
                Ok(json!({}))
            }
            async fn record(&self, _on: bool) -> Result<Value, String> {
                Err("n/a".into())
            }
            async fn record_pause(&self, _on: bool) -> Result<(), String> {
                Ok(())
            }
            async fn monitoring(&self) -> Result<Value, String> {
                Err("n/a".into())
            }
            async fn stream_start(&self, _s: String, _k: String, _t: Vec<(u32, u32, u32)>) -> Result<Value, String> {
                Err("n/a".into())
            }
            async fn stream_stop(&self) -> Result<(), String> {
                Ok(())
            }
            async fn stream_stats(&self) -> Result<Value, String> {
                Ok(Value::Null)
            }
            async fn obs_collections(&self) -> Result<Value, String> {
                Err("n/a".into())
            }
            async fn import_obs(&self, _id: String) -> Result<Value, String> {
                Err("n/a".into())
            }
            async fn set_live_quality(&self, pct: u8) -> Result<(), String> {
                self.legacy_calls.lock().unwrap().push(pct);
                Ok(())
            }
            async fn set_group_live_quality(&self, id: &str, pct: u8) -> Result<(), String> {
                self.group_calls.lock().unwrap().push((id.to_string(), pct));
                Ok(())
            }
            async fn group_stats(&self) -> Result<Option<Value>, String> {
                Ok(Some(self.stats.lock().unwrap().clone()))
            }
            async fn host_budget_info(&self) -> Option<HostBudgetInfo> {
                self.budget.lock().unwrap().clone()
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let e = Engine::start_with_secrets(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default()))
            .await
            .unwrap();
        let host = Arc::new(GroupMockHost {
            budget: Mutex::new(Some(HostBudgetInfo {
                os: "windows".into(),
                encoder_id: "ffmpeg_nvenc".into(),
                cpu_cores: 8,
                nvidia_driver_major: Some(525),
                host_sig: "win|nv=525".into(),
                ..Default::default()
            })),
            stats: Mutex::new(json!({ GROUP_MAIN: { "dropped": 0 }, "g_kick": { "dropped": 0 } })),
            group_calls: Mutex::new(Vec::new()),
            legacy_calls: Mutex::new(Vec::new()),
        });
        e.set_studio_host(host.clone());
        let d_yt = e.add_golive_destination("YT", "rtmp://127.0.0.1:19901/app", "k1", Canvas::Main).await.unwrap();
        let d_kick = e.add_golive_destination("Kick", "rtmp://127.0.0.1:19902/app", "k2", Canvas::Main).await.unwrap();
        let d_fb = e.add_golive_destination("FB", "rtmp://127.0.0.1:19903/app", "k3", Canvas::Main).await.unwrap();

        let _yt_rx = e.replace_running_health_for_test(&d_yt.id, GROUP_MAIN, 12_000, 60_000);
        let kick_rx = e.replace_running_health_for_test(&d_kick.id, "g_kick", 6_000, 60_000);
        let _ = e.refresh_pc_budget().await;

        // 2 active groups against NVENC max=3 -> spare handover session exists; g_kick drops trigger ONLY g_kick step-down.
        let t0 = now().0;
        e.auto_quality_tick_at(t0, 0).await;
        kick_rx.send_modify(|h| h.dropped = 50);
        e.auto_quality_tick_at(t0 + 12_000, 0).await;

        let calls = host.group_calls.lock().unwrap().clone();
        assert_eq!(calls, vec![("g_kick".to_string(), 85)], "only struggling group should step down: {calls:?}");
        assert!(host.legacy_calls.lock().unwrap().is_empty());

        // Exhaust NVENC session cap (3 groups >= 3 sessions on driver <531): further step-downs must be blocked.
        let fb_rx = e.replace_running_health_for_test(&d_fb.id, "g_fb", 4_000, 60_000);
        e.auto_quality_tick_at(t0 + 15_000, 0).await;
        fb_rx.send_modify(|h| h.dropped = 50);
        host.group_calls.lock().unwrap().clear();
        e.auto_quality_tick_at(t0 + 30_000, 0).await;
        assert!(
            host.group_calls.lock().unwrap().is_empty(),
            "exhausted NVENC session cap must skip FFmpeg handover bitrate change: {:?}",
            host.group_calls.lock().unwrap()
        );

        // Disabling auto_quality restores lowered group (`g_kick`) to 100%.
        e.auto_quality_off().await;
        assert!(host.group_calls.lock().unwrap().contains(&("g_kick".to_string(), 100)));
    }
}
