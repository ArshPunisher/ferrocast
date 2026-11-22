//! Live label evaluation, monthly boundary caching, SQLite hydration, and hype/combo train
//! management in [`crate::Engine`].

use std::collections::HashMap;
use std::time::Duration;

use ferrocast_core::event::{Event, EventKind, Millis, Origin};
use ferrocast_core::labels::{
    ComboTrain, ComboTrainConfig, ComboTrainView, LatestLabels, PeriodTotals, format_thousands, month_bounds,
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

/// Engine-held state for local stream labels (session, monthly cache, all-time, latest-X,
/// and active hype/combo trains).
#[derive(Debug, Default)]
pub(crate) struct LabelRuntime {
    hydrated: bool,
    latest: LatestLabels,
    session: PeriodTotals,
    alltime: PeriodTotals,
    monthly: HashMap<i32, ((Millis, Millis), PeriodTotals)>,
    trains: HashMap<TrainKey, ComboTrain>,
}

impl LabelRuntime {
    fn ensure_train(&mut self, cfg: ComboTrainConfig) -> &mut ComboTrain {
        let key = TrainKey::from(cfg);
        if !self.trains.contains_key(&key) && self.trains.len() >= MAX_TRAINS {
            // Keep trains bounded under adversarial configs by keeping idle trains clean.
            if let Some(idle_key) =
                self.trains.iter().find(|(_, t)| !t.view(Millis(i64::MAX / 2)).active).map(|(k, _)| *k)
            {
                self.trains.remove(&idle_key);
            }
        }
        self.trains.entry(key).or_insert_with(|| ComboTrain::new(cfg))
    }

    fn observe_event(&mut self, event: &Event, now_ms: Millis) {
        if event.origin != Origin::Live {
            return;
        }
        self.latest.observe(event);
        self.session.observe(event);
        self.alltime.observe(event);

        // Update cached months matching `event.occurred_at` (or always pass reversals cleanly).
        let is_reversal = matches!(event.kind, EventKind::Reversal { .. });
        self.monthly.retain(|tz, ((start, end), totals)| {
            let (cur_start, cur_end) = month_bounds(now_ms, *tz);
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
            train.observe(event);
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

    fn has_active_trains(&self, now_ms: Millis) -> bool {
        self.trains.values().any(|t| t.view(now_ms).active)
    }
}

impl Engine {
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
        rt.monthly.insert(0, ((m_start, m_end), monthly));
        rt.ensure_train(ComboTrainConfig::default());
        rt.hydrated = true;
    }

    async fn ensure_month_cached(&self, tz_offset_min: i32, now_ms: Millis) {
        let bounds = month_bounds(now_ms, tz_offset_min);
        {
            let rt = self.inner.labels.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((cached_bounds, _)) = rt.monthly.get(&tz_offset_min)
                && *cached_bounds == bounds
            {
                return;
            }
        }
        let (start, end) = bounds;
        if let Ok(totals) = self.db(move |s| repo::labels::period_totals(s, Some(start.0), Some(end.0))).await {
            let mut rt = self.inner.labels.lock().unwrap_or_else(|p| p.into_inner());
            rt.monthly.insert(tz_offset_min, (bounds, totals));
        }
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
                        t.observe(event);
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

        // Gather all monthly timezone offsets and hype train configs needed by widgets on `o`.
        let mut tz_needed = Vec::new();
        let mut train_cfgs = Vec::new();
        for w in o.widgets.iter().filter(|w| w.kind == WidgetKind::Label) {
            if let Ok(opts) = validate_label(&w.config) {
                if opts.source.is_monthly() && !tz_needed.contains(&opts.timezone_offset_min) {
                    tz_needed.push(opts.timezone_offset_min);
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
            rt.ensure_train(cfg);
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

            let raw = eval_non_train(&opts, &rt);
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
        let Ok(overlays) = self.db(repo::overlays::list).await else { return };
        for o in overlays {
            if o.widgets.iter().any(|w| w.kind == WidgetKind::Label) {
                self.push_overlay_labels_for(&o).await;
            }
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
}

fn eval_non_train(opts: &LabelOptions, rt: &LabelRuntime) -> String {
    if let Some(latest) = rt.latest.label(opts.source) {
        return latest;
    }
    let month_totals = rt.monthly.get(&opts.timezone_offset_min).map(|(_, t)| t);
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
        let now_ms = crate::now();
        let expired = {
            let mut rt = engine.inner.labels.lock().unwrap_or_else(|p| p.into_inner());
            if !rt.has_active_trains(now_ms) { false } else { rt.tick_trains(now_ms) }
        };
        if expired {
            engine.push_all_overlay_labels().await;
        }
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
}
