//! Live label evaluation, monthly boundary caching, SQLite hydration, and hype/combo train
//! management in [`crate::Engine`].

use std::collections::HashMap;
use std::time::Duration;

use ferrocast_core::event::{Event, EventKind, Millis, Origin};
use ferrocast_core::labels::{
    ComboTrain, ComboTrainConfig, ComboTrainView, LatestLabels, PeriodTotals, format_thousands, is_supported_timezone,
    month_bounds, month_bounds_for_zone,
};
use ferrocast_core::overlay::{LabelOptions, LabelSource, Overlay, WidgetKind, validate_label};
use ferrocast_core::template::{Template, Vars};
use ferrocast_store::repo;
use serde_json::{Map, Value, json};

use crate::Engine;
use crate::frames::{Audience, OverlayFrame};

const MAX_TRAINS: usize = 24;
const REAPER_TICK: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TrainKey {
    window_s: u32,
    step_s: u32,
    max_s: u32,
    threshold: u32,
}

impl From<ComboTrainConfig> for TrainKey {
    fn from(c: ComboTrainConfig) -> Self {
        Self { window_s: c.window_s, step_s: c.step_s, max_s: c.max_s, threshold: c.threshold }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum MonthZoneKey {
    Offset(i32),
    Zone(String),
}

impl MonthZoneKey {
    fn from_opts(opts: &LabelOptions) -> Self {
        match opts.timezone.as_deref() {
            Some(tz) if is_supported_timezone(tz) => Self::Zone(tz.to_string()),
            _ => Self::Offset(opts.timezone_offset_min),
        }
    }

    fn bounds(&self, at: Millis) -> (Millis, Millis) {
        match self {
            Self::Offset(off) => month_bounds(at, *off),
            Self::Zone(tz) => month_bounds_for_zone(at, Some(tz), 0),
        }
    }
}

/// Engine-held state for local stream labels (session, monthly cache, all-time, latest-X,
/// and active hype/combo trains).
#[derive(Debug, Default)]
pub(crate) struct LabelRuntime {
    hydrated: bool,
    latest: LatestLabels,
    session: PeriodTotals,
    alltime: PeriodTotals,
    monthly: HashMap<MonthZoneKey, ((Millis, Millis), PeriodTotals)>,
    trains: HashMap<TrainKey, ComboTrain>,
    /// Monthly totals being loaded from the log right now. Live events seen meanwhile are kept
    /// so they can be reconciled with what the load did or did not count.
    fills_in_flight: usize,
    fill_buffer: Vec<Event>,
}

impl LabelRuntime {
    /// The train for `cfg`, made if it isn't there. `None` when the limit is reached and every
    /// existing train is active: nothing live is evicted, and the new configuration is refused
    /// (its label shows no train) rather than adding state and per-event work without bound.
    fn ensure_train(&mut self, cfg: ComboTrainConfig, now_ms: Millis) -> Option<&mut ComboTrain> {
        let key = TrainKey::from(cfg);
        if !self.trains.contains_key(&key) && self.trains.len() >= MAX_TRAINS {
            let idle_key = self.trains.iter().find(|(_, t)| !t.view(now_ms).active).map(|(k, _)| *k)?;
            self.trains.remove(&idle_key);
        }
        Some(self.trains.entry(key).or_insert_with(|| ComboTrain::new(cfg)))
    }

    fn observe_event(&mut self, event: &Event, now_ms: Millis) {
        if event.origin != Origin::Live {
            return;
        }
        self.latest.observe(event);
        self.session.observe(event);
        self.alltime.observe(event);
        if self.fills_in_flight > 0 && self.fill_buffer.len() < 10_000 {
            self.fill_buffer.push(event.clone());
        }

        // Update cached months matching `event.occurred_at` (or always pass reversals cleanly).
        let is_reversal = matches!(event.kind, EventKind::Reversal { .. });
        self.monthly.retain(|tz_key, ((start, end), totals)| {
            let (cur_start, cur_end) = tz_key.bounds(now_ms);
            if cur_start != *start || cur_end != *end {
                // Month rolled over: drop stale cache so next eval reloads [cur_start, cur_end).
                return false;
            }
            if is_reversal || (event.occurred_at.0 >= start.0 && event.occurred_at.0 < end.0) {
                totals.observe(event);
            }
            true
        });

        // Default train always active alongside any custom overlay configs.
        if self.trains.is_empty() {
            self.trains
                .insert(TrainKey::from(ComboTrainConfig::default()), ComboTrain::new(ComboTrainConfig::default()));
        }
        for train in self.trains.values_mut() {
            train.observe_at(event, now_ms);
        }
    }

    /// Installs a freshly loaded month: totals that count every event up to `watermark` (log
    /// `seq`), plus the live events seen while the load ran that the load could not have counted.
    fn finish_month_fill(
        &mut self,
        key: MonthZoneKey,
        bounds: (Millis, Millis),
        mut totals: PeriodTotals,
        watermark: u64,
    ) {
        let (start, end) = bounds;
        for ev in &self.fill_buffer {
            let in_month = ev.occurred_at.0 >= start.0 && ev.occurred_at.0 < end.0;
            if ev.seq > watermark && (in_month || matches!(ev.kind, EventKind::Reversal { .. })) {
                totals.observe(ev);
            }
        }
        self.monthly.insert(key, (bounds, totals));
    }

    fn end_fill(&mut self) {
        self.fills_in_flight = self.fills_in_flight.saturating_sub(1);
        if self.fills_in_flight == 0 {
            self.fill_buffer.clear();
        }
    }

    fn reset_session(&mut self) {
        self.session = PeriodTotals::default();
    }

    fn tick_trains(&mut self, now_ms: Millis) -> bool {
        let mut any_expired = false;
        for train in self.trains.values_mut() {
            if train.tick(now_ms) {
                any_expired = true;
            }
        }
        any_expired
    }

    fn rollover_months(&mut self, now_ms: Millis) -> bool {
        let before = self.monthly.len();
        self.monthly.retain(|tz_key, ((start, end), _)| tz_key.bounds(now_ms) == (*start, *end));
        self.monthly.len() != before
    }

    fn has_active_trains(&self, now_ms: Millis) -> bool {
        self.trains.values().any(|t| t.view(now_ms).active)
    }
}

impl Engine {
    /// Checks idle train expiration (and monthly rollover) for a single reaper tick at `now_ms`.
    pub(crate) async fn labels_reap_tick(&self, now_ms: Millis) -> bool {
        let changed = {
            let mut rt = self.inner.labels.lock().unwrap_or_else(|p| p.into_inner());
            let train_expired = if !rt.has_active_trains(now_ms) { false } else { rt.tick_trains(now_ms) };
            let month_rolled = rt.rollover_months(now_ms);
            train_expired || month_rolled
        };
        if changed {
            self.push_all_overlay_labels().await;
        }
        changed
    }

    /// Loads `latest_labels`, `alltime` totals, and UTC monthly totals from SQLite on startup.
    pub(crate) async fn hydrate_labels(&self) {
        let now_ms = crate::now();
        let (m_start, m_end) = month_bounds(now_ms, 0);
        let res = self
            .db(move |s| {
                let latest = repo::labels::latest_labels(s)?;
                let alltime = repo::labels::period_totals(s, None, None)?;
                let monthly = repo::labels::period_totals(s, Some(m_start.0), Some(m_end.0))?;
                Ok((latest, alltime, monthly))
            })
            .await;

        let Ok((latest, alltime, monthly)) = res else {
            tracing::warn!("couldn't hydrate local stream labels from store");
            return;
        };

        let mut rt = self.inner.labels.lock().unwrap_or_else(|p| p.into_inner());
        rt.latest = latest;
        rt.alltime = alltime;
        rt.monthly.insert(MonthZoneKey::Offset(0), ((m_start, m_end), monthly));
        let _ = rt.ensure_train(ComboTrainConfig::default(), now_ms);
        rt.hydrated = true;
    }

    async fn ensure_month_cached(&self, key: MonthZoneKey, now_ms: Millis) {
        let bounds = key.bounds(now_ms);
        {
            let rt = self.inner.labels.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((cached_bounds, _)) = rt.monthly.get(&key)
                && *cached_bounds == bounds
            {
                return;
            }
        }
        let (start, end) = bounds;
        self.inner.labels.lock().unwrap_or_else(|p| p.into_inner()).fills_in_flight += 1;
        let loaded = self.db(move |s| repo::labels::period_totals_with_watermark(s, Some(start.0), Some(end.0))).await;
        let mut rt = self.inner.labels.lock().unwrap_or_else(|p| p.into_inner());
        if let Ok((totals, watermark)) = loaded {
            rt.finish_month_fill(key, bounds, totals, watermark);
        }
        rt.end_fill();
    }

    pub(crate) async fn labels_reset_session(&self) {
        let mut rt = self.inner.labels.lock().unwrap_or_else(|p| p.into_inner());
        rt.reset_session();
    }

    pub(crate) async fn labels_observe_event(&self, event: &Event) {
        let hydrated = self.inner.labels.lock().unwrap_or_else(|p| p.into_inner()).hydrated;
        if !hydrated {
            self.hydrate_labels().await;
            // After hydrating from DB, the DB already contains this event if pipeline persisted
            // it first. Refresh session + trains cleanly; note pipeline persists BEFORE support_event!
        }
        let now_ms = crate::now();
        let mut needs_tip_rehydrate = false;
        {
            let mut rt = self.inner.labels.lock().unwrap_or_else(|p| p.into_inner());
            // Ensure train sees live events even if hydrate_labels happened after DB insert.
            if event.origin == Origin::Live {
                // Only update counters incrementally when we didn't literally just hydrate after DB write!
                if hydrated {
                    rt.observe_event(event, now_ms);
                    if matches!(event.kind, EventKind::Reversal { .. }) && rt.latest.tip.is_none() {
                        needs_tip_rehydrate = true;
                    }
                } else {
                    rt.session.observe(event);
                    for t in rt.trains.values_mut() {
                        t.observe_at(event, now_ms);
                    }
                }
            }
        }
        if needs_tip_rehydrate && let Ok(refreshed) = self.db(repo::labels::latest_labels).await {
            let mut rt = self.inner.labels.lock().unwrap_or_else(|p| p.into_inner());
            rt.latest.tip = refreshed.tip;
        }
        self.push_all_overlay_labels().await;
    }

    /// Evaluates `{ labels, label_trains }` for a single overlay at `now_ms`.
    pub(crate) async fn overlay_labels_and_trains(&self, o: &Overlay) -> (Map<String, Value>, Map<String, Value>) {
        let now_ms = crate::now();
        let hydrated = self.inner.labels.lock().unwrap_or_else(|p| p.into_inner()).hydrated;
        if !hydrated {
            self.hydrate_labels().await;
        }

        // Gather all monthly timezones and hype train configs needed by widgets on `o`.
        let mut tz_needed = Vec::new();
        let mut train_cfgs = Vec::new();
        for w in o.widgets.iter().filter(|w| w.kind == WidgetKind::Label) {
            if let Ok(opts) = validate_label(&w.config) {
                if opts.source.is_monthly() {
                    let key = MonthZoneKey::from_opts(&opts);
                    if !tz_needed.contains(&key) {
                        tz_needed.push(key);
                    }
                }
                if opts.source.is_train() {
                    train_cfgs.push(ComboTrainConfig {
                        window_s: opts.train_window_s,
                        step_s: opts.train_step_s,
                        max_s: opts.train_max_s,
                        threshold: opts.train_threshold,
                    });
                }
            }
        }

        for tz in tz_needed {
            self.ensure_month_cached(tz, now_ms).await;
        }

        let mut rt = self.inner.labels.lock().unwrap_or_else(|p| p.into_inner());
        for cfg in train_cfgs {
            let _ = rt.ensure_train(cfg, now_ms);
        }

        let mut counter_vars = Vars::new();
        self.counter_vars(&mut counter_vars);

        let mut labels = Map::new();
        let mut trains = Map::new();

        for w in o.widgets.iter().filter(|w| w.kind == WidgetKind::Label) {
            let Ok(opts) = validate_label(&w.config) else {
                // Legacy/unknown label source fallback
                let source = w.config.get("source").and_then(Value::as_str).unwrap_or("");
                let tpl = w.config.get("template").and_then(Value::as_str).unwrap_or("{value}");
                let mut vars = counter_vars.clone();
                let val = self.state().session.label(source);
                vars.insert("value".into(), val.clone());
                vars.insert("name".into(), val);
                let rendered = Template::parse(tpl).map(|t| t.render(&vars)).unwrap_or_default();
                labels.insert(w.id.clone(), json!(rendered));
                continue;
            };

            if opts.source == LabelSource::HypeTrain {
                let cfg = ComboTrainConfig {
                    window_s: opts.train_window_s,
                    step_s: opts.train_step_s,
                    max_s: opts.train_max_s,
                    threshold: opts.train_threshold,
                };
                let key = TrainKey::from(cfg);
                let (rendered, view) = match rt.trains.get(&key) {
                    Some(train) => (train.render(&opts.template, now_ms), train.view(now_ms)),
                    None => (String::new(), ComboTrainView { threshold: cfg.threshold, ..Default::default() }),
                };
                labels.insert(w.id.clone(), json!(rendered));
                if view.active {
                    trains.insert(w.id.clone(), serde_json::to_value(view).unwrap_or(Value::Null));
                } else {
                    trains.insert(w.id.clone(), Value::Null);
                }
                continue;
            }

            let raw = eval_non_train(&opts, &rt, now_ms);
            let mut vars = counter_vars.clone();
            vars.insert("value".into(), raw.clone());
            vars.insert("name".into(), raw.clone());
            let rendered = Template::parse(&opts.template).map(|t| t.render(&vars)).unwrap_or_default();
            labels.insert(w.id.clone(), json!(rendered));
        }

        (labels, trains)
    }

    /// Broadcasts fresh label values (and active hype train timers) to every overlay with labels.
    pub(crate) async fn push_all_overlay_labels(&self) {
        let Ok(overlays) = self.db(|s| repo::overlays::list_with_widget_kind(s, "label")).await else { return };
        for o in overlays {
            self.push_overlay_labels_for(&o).await;
        }
    }

    pub(crate) async fn push_overlay_labels_for(&self, o: &Overlay) {
        if !o.widgets.iter().any(|w| w.kind == WidgetKind::Label) {
            return;
        }
        let (labels, label_trains) = self.overlay_labels_and_trains(o).await;
        self.send_overlay(OverlayFrame::new(
            Audience::Overlay(o.id.clone()),
            self.next_seq(),
            "state",
            &json!({ "labels": labels, "label_trains": label_trains }),
        ));
    }

    /// Evaluates every stream label source for dashboard previews (`GET /api/v1/labels`) in UTC.
    /// Returns an empty map when no activity has been recorded yet so previews can show samples.
    pub async fn all_labels(&self) -> std::collections::BTreeMap<String, String> {
        self.all_labels_for_offset(0).await
    }

    /// Evaluates every stream label source for dashboard previews using `tz_offset_min` (`-720..=840`)
    /// for calendar-month rollovers.
    pub async fn all_labels_for_offset(&self, tz_offset_min: i32) -> std::collections::BTreeMap<String, String> {
        let tz_offset_min = tz_offset_min.clamp(-720, 840);
        let now_ms = crate::now();
        let hydrated = self.inner.labels.lock().unwrap_or_else(|p| p.into_inner()).hydrated;
        if !hydrated {
            self.hydrate_labels().await;
        }
        let default_opts = |source: LabelSource| LabelOptions {
            source,
            template: "{value}".into(),
            theme: "molten".into(),
            timezone: None,
            timezone_offset_min: tz_offset_min,
            train_window_s: 120,
            train_step_s: 15,
            train_max_s: 300,
            train_threshold: 3,
        };
        let tz = MonthZoneKey::from_opts(&default_opts(LabelSource::MonthlyFollows));
        self.ensure_month_cached(tz, now_ms).await;

        let rt = self.inner.labels.lock().unwrap_or_else(|p| p.into_inner());
        let mut out = std::collections::BTreeMap::new();
        let mut has_activity = false;
        for &source in LabelSource::ALL {
            let val = if source == LabelSource::HypeTrain {
                rt.trains
                    .values()
                    .find(|t| t.view(now_ms).active)
                    .map(|t| t.render("Lv.{level} · {events} events · {timer}", now_ms))
                    .unwrap_or_default()
            } else {
                eval_non_train(&default_opts(source), &rt, now_ms)
            };
            if val.is_empty() {
                continue;
            }
            if val != "0" {
                has_activity = true;
            }
            out.insert(source.as_str().to_string(), val);
        }
        if has_activity { out } else { std::collections::BTreeMap::new() }
    }
}

fn eval_non_train(opts: &LabelOptions, rt: &LabelRuntime, now_ms: Millis) -> String {
    if let Some(latest) = rt.latest.label(opts.source) {
        return latest;
    }
    // Only a total for the month it is now: after a failed refresh the cache can still hold
    // last month's, which must not show as this month's.
    let key = MonthZoneKey::from_opts(opts);
    let month_totals = rt.monthly.get(&key).filter(|(bounds, _)| *bounds == key.bounds(now_ms)).map(|(_, t)| t);
    match opts.source {
        LabelSource::SessionFollows => rt.session.follows.to_string(),
        LabelSource::SessionSubs => rt.session.subs.to_string(),
        LabelSource::SessionBits => format_thousands(rt.session.bits),
        LabelSource::SessionTips => rt.session.formatted_tips(),
        LabelSource::SessionRaids => rt.session.raids.to_string(),
        LabelSource::TopTipSession => rt.session.top_tip_display(),
        LabelSource::TopSupporterSession => rt.session.top_supporter_display(),
        LabelSource::MonthlyFollows => month_totals.map_or(0, |t| t.follows).to_string(),
        LabelSource::MonthlySubs => month_totals.map_or(0, |t| t.subs).to_string(),
        LabelSource::MonthlyBits => format_thousands(month_totals.map_or(0, |t| t.bits)),
        LabelSource::MonthlyTips => month_totals.map_or_else(|| "0".into(), PeriodTotals::formatted_tips),
        LabelSource::MonthlyRaids => month_totals.map_or(0, |t| t.raids).to_string(),
        LabelSource::TopTipMonthly => month_totals.map(PeriodTotals::top_tip_display).unwrap_or_default(),
        LabelSource::TopSupporterMonthly => month_totals.map(PeriodTotals::top_supporter_display).unwrap_or_default(),
        LabelSource::AlltimeFollows => rt.alltime.follows.to_string(),
        LabelSource::AlltimeSubs => rt.alltime.subs.to_string(),
        LabelSource::AlltimeBits => format_thousands(rt.alltime.bits),
        LabelSource::AlltimeTips => rt.alltime.formatted_tips(),
        LabelSource::AlltimeRaids => rt.alltime.raids.to_string(),
        LabelSource::TopTipAlltime => rt.alltime.top_tip_display(),
        LabelSource::TopSupporterAlltime => rt.alltime.top_supporter_display(),
        _ => String::new(),
    }
}

/// Expires active hype trains as soon as their countdown hits zero and updates connected overlays.
pub(crate) async fn run(engine: Engine) {
    loop {
        tokio::time::sleep(REAPER_TICK).await;
        engine.labels_reap_tick(crate::now()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrocast_core::event::{Actor, PaymentStatus, Platform, ReversalReason, TipProvider};
    use ferrocast_core::money::Money;
    use ferrocast_store::repo::events;

    #[tokio::test]
    async fn live_reversal_of_hydrated_tip_falls_back_to_prior_captured_tip() {
        let dir = tempfile::tempdir().unwrap();
        let engine1 = Engine::start_with_memory_secrets(dir.path()).await.unwrap();
        let actor = |n: &str| Actor::named(Platform::Tips, n);
        let tip = |id: &str, name: &str, cents: i64, at: i64| {
            let mut e = Event::live(
                Platform::Tips,
                Some(id.into()),
                Millis(at),
                EventKind::Tip {
                    actor: actor(name),
                    money: Money::new(cents, "USD"),
                    message: None,
                    provider: TipProvider::Stripe,
                    payment_ref: id.into(),
                    status: PaymentStatus::Captured,
                },
            );
            e.id = id.into();
            e
        };
        let t1 = tip("t1", "OlderTipper", 1500, 1_700_000_000_000);
        let t2 = tip("t2", "RefundedTipper", 9000, 1_700_000_001_000);
        engine1
            .db(move |s| {
                events::insert(s, &t1)?;
                events::insert(s, &t2)?;
                Ok(())
            })
            .await
            .unwrap();
        engine1.shutdown();

        // Simulate restart: hydrate_labels loads t2; then t2 gets reversed live -> falls back to t1!
        let engine2 = Engine::start_with_memory_secrets(dir.path()).await.unwrap();
        engine2.hydrate_labels().await;
        {
            let rt = engine2.inner.labels.lock().unwrap();
            assert_eq!(rt.latest.label(LabelSource::LatestTip).as_deref(), Some("RefundedTipper $90"));
        }
        let mut rev = Event::live(
            Platform::Tips,
            Some("rev-t2".into()),
            Millis(1_700_000_002_000),
            EventKind::Reversal { original_source_id: "t2".into(), reason: ReversalReason::Refund },
        );
        rev.id = "rev-t2".into();
        let rev_ins = rev.clone();
        engine2.db(move |s| events::insert(s, &rev_ins).map(|_| ())).await.unwrap();
        engine2.labels_observe_event(&rev).await;

        let rt = engine2.inner.labels.lock().unwrap();
        assert_eq!(rt.latest.label(LabelSource::LatestTip).as_deref(), Some("OlderTipper $15"));
    }

    #[tokio::test]
    async fn hype_train_anchors_expiry_to_local_clock_and_protects_active_train_under_cap() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::start_with_memory_secrets(dir.path()).await.unwrap();
        engine.hydrate_labels().await;

        let now_ms = crate::now();
        let cfg = ComboTrainConfig { window_s: 60, step_s: 15, max_s: 90, threshold: 2 };
        {
            let mut rt = engine.inner.labels.lock().unwrap();
            let _ = rt.ensure_train(cfg, now_ms);
            // Skewed platform timestamps 5 minutes in the past must still ignite using local now_ms
            for (i, who) in ["Fan1", "Fan2"].into_iter().enumerate() {
                let mut e = Event::live(
                    Platform::Twitch,
                    Some(format!("skew-{i}")),
                    Millis(now_ms.0 - 300_000 + i as i64 * 1_000),
                    EventKind::Follow { actor: Actor::named(Platform::Twitch, who) },
                );
                e.id = format!("skew-{i}");
                rt.observe_event(&e, now_ms);
            }
            let view = rt.trains.get(&TrainKey::from(cfg)).unwrap().view(now_ms);
            assert!(view.active, "train must be active at local now_ms even with 5m platform clock lag");
            assert!(view.ends_at.unwrap().0 > now_ms.0);
        }

        // Fill MAX_TRAINS other configs and verify the active train is NOT evicted!
        let mut rt = engine.inner.labels.lock().unwrap();
        for w in 10..=(10 + MAX_TRAINS as u32 + 2) {
            let _ = rt.ensure_train(ComboTrainConfig { window_s: w, step_s: 15, max_s: 120, threshold: 2 }, now_ms);
        }
        let active_key = TrainKey::from(cfg);
        assert!(rt.trains.contains_key(&active_key), "active train must not be evicted as idle");
        assert!(rt.trains.get(&active_key).unwrap().view(now_ms).active);
    }

    #[tokio::test]
    async fn idle_month_rollover_evicts_stale_monthly_cache_and_pushes_update() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::start_with_memory_secrets(dir.path()).await.unwrap();
        let view = engine.create_overlay("MonthRollover", ferrocast_core::overlay::Preset::Blank).await.unwrap();
        let mut ov = engine.overlay(&view.id).await.unwrap();
        ov.widgets.push(ferrocast_core::overlay::Widget::new(
            "mf",
            WidgetKind::Label,
            (10, 10, 200, 40),
            json!({ "source": "monthly_follows", "template": "{value}", "theme": "molten", "timezone_offset_min": 0 }),
        ));
        engine.update_overlay(ov.clone()).await.unwrap();
        let mut rx = engine.subscribe_overlays();

        // Seed stale January 2026 monthly cache
        let jan_ts = Millis(1_769_900_000_000); // 2026-01-31T22:53:20Z
        let jan_bounds = month_bounds(jan_ts, 0);
        let feb_ts = Millis(jan_bounds.1.0 + 1_000); // 2026-02-01T00:00:01Z
        {
            let mut rt = engine.inner.labels.lock().unwrap();
            let mut jan_totals = PeriodTotals::default();
            jan_totals.follows = 42;
            rt.monthly.insert(MonthZoneKey::Offset(0), (jan_bounds, jan_totals));
            rt.hydrated = true;
        }

        // Advance reaper into February without any new events arriving
        let changed = engine.labels_reap_tick(feb_ts).await;
        assert!(changed, "month rollover must trigger reaper push");
        {
            let rt = engine.inner.labels.lock().unwrap();
            assert_ne!(
                rt.monthly.get(&MonthZoneKey::Offset(0)).map(|(b, _)| *b),
                Some(jan_bounds),
                "stale January bounds must be evicted/replaced"
            );
        }
        let mut pushed_mf = None;
        while let Ok(frame) = rx.try_recv() {
            let v: Value = serde_json::from_str(&frame.text).unwrap();
            if v["type"] == "state"
                && let Some(val) = v["data"]["labels"]["mf"].as_str()
            {
                pushed_mf = Some(val.to_string());
            }
        }
        assert_eq!(pushed_mf.as_deref(), Some("0"), "overlay must receive rolled-over monthly value");
    }

    fn follow_event(seq: u64, at: i64) -> Event {
        let mut e = Event::live(
            Platform::Twitch,
            Some(format!("f-{seq}")),
            Millis(at),
            EventKind::Follow { actor: Actor::named(Platform::Twitch, "Viewer") },
        );
        e.seq = seq;
        e
    }

    #[test]
    fn a_monthly_load_adds_only_the_live_events_it_could_not_have_counted() {
        let mut rt = LabelRuntime::default();
        let key = MonthZoneKey::Offset(0);
        let bounds = (Millis(1_000), Millis(9_000));
        rt.fills_in_flight = 1;
        // Seen while the load ran: seq 5 was already in the log when it read (counted), seq 6
        // was not, and seq 7 belongs to another month.
        for ev in [follow_event(5, 2_000), follow_event(6, 3_000), follow_event(7, 20_000)] {
            rt.observe_event(&ev, Millis(3_500));
        }
        let loaded = ferrocast_core::labels::summarize_events(&[follow_event(1, 1_500), follow_event(5, 2_000)]);
        rt.finish_month_fill(key.clone(), bounds, loaded, 5);
        rt.end_fill();
        assert_eq!(rt.monthly.get(&key).unwrap().1.follows, 3, "1, 5 from the log, plus 6 seen live");
        assert!(rt.fill_buffer.is_empty(), "nothing is kept once no load is running");
    }

    #[test]
    fn the_train_limit_holds_when_every_train_is_active() {
        let now = Millis(1_000_000);
        let mut rt = LabelRuntime::default();
        let live = |i: u32| ComboTrainConfig { window_s: 60 + i, step_s: 15, max_s: 300, threshold: 1 };
        for i in 0..MAX_TRAINS as u32 {
            let t = rt.ensure_train(live(i), now).expect("room");
            let mut e = Event::live(
                Platform::Twitch,
                Some(format!("e{i}")),
                now,
                EventKind::Follow { actor: Actor::named(Platform::Twitch, "A") },
            );
            e.id = format!("e{i}");
            t.observe_at(&e, now);
            assert!(t.view(now).active);
        }
        assert!(rt.ensure_train(live(99), now).is_none(), "a 25th active configuration is refused");
        assert_eq!(rt.trains.len(), MAX_TRAINS);
    }

    #[test]
    fn last_months_cached_total_is_not_shown_as_this_months() {
        let now = Millis(1_000_000_000_000);
        let key = MonthZoneKey::Offset(0);
        let (cur_start, cur_end) = key.bounds(now);
        let mut rt = LabelRuntime::default();
        let mut old = PeriodTotals::default();
        old.follows = 7;
        // The cache still holds an entry for the month before.
        rt.monthly.insert(key.clone(), ((Millis(cur_start.0 - 2_678_400_000), cur_start), old));
        let opts =
            validate_label(&json!({ "source": "monthly_follows", "template": "{value}", "timezone_offset_min": 0 }))
                .unwrap();
        assert_eq!(eval_non_train(&opts, &rt, now), "0");
        let mut fresh = PeriodTotals::default();
        fresh.follows = 2;
        rt.monthly.insert(key, ((cur_start, cur_end), fresh));
        assert_eq!(eval_non_train(&opts, &rt, now), "2");
    }
}
