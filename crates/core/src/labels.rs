//! Local stream label maths, calendar month boundaries, supporter ranking, and hype/combo train
//! state machines.
//!
//! Pure core module: never does I/O or reads clocks. Callers supply timestamps via [`Millis`].

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::event::{Event, EventKind, Millis, Origin, PaymentStatus, Platform};
use crate::money::{Money, ValueUnit, exponent};
use crate::overlay::LabelSource;
use crate::template::{Template, Vars};

const MS_PER_MIN: i64 = 60_000;
const MS_PER_HOUR: i64 = 3_600_000;
const MS_PER_DAY: i64 = 86_400_000;

/// Supported IANA timezones with deterministic DST rules in pure core without external clock crates.
pub const SUPPORTED_TIMEZONES: &[&str] = &[
    "UTC",
    "Asia/Kolkata",
    "Asia/Tokyo",
    "Europe/London",
    "Europe/Berlin",
    "America/New_York",
    "America/Chicago",
    "America/Los_Angeles",
    "Australia/Sydney",
];

/// Returns `true` if `tz` is a supported timezone identifier.
pub fn is_supported_timezone(tz: &str) -> bool {
    SUPPORTED_TIMEZONES.contains(&tz)
}

/// Returns `(month_start_utc, next_month_start_utc)` in half-open `[start, end)` form for the
/// calendar month containing `at` under fixed UTC offset `tz_offset_min` (`-720..=840`).
///
/// Uses Howard Hinnant's exact civil Gregorian calendar algorithms without external clock crates.
pub fn month_bounds(at: Millis, tz_offset_min: i32) -> (Millis, Millis) {
    let offset_ms = i64::from(tz_offset_min.clamp(-720, 840)) * MS_PER_MIN;
    let shifted = at.0.saturating_add(offset_ms);
    let days = shifted.div_euclid(MS_PER_DAY);
    let (year, month, _) = civil_from_days(days);
    let start_days = days_from_civil(year, month, 1);
    let (next_year, next_month) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    let end_days = days_from_civil(next_year, next_month, 1);
    (Millis(start_days * MS_PER_DAY - offset_ms), Millis(end_days * MS_PER_DAY - offset_ms))
}

/// Returns `(month_start_utc, next_month_start_utc)` using DST-aware `timezone` when supplied and
/// supported, falling back to fixed `fallback_offset_min` otherwise.
pub fn month_bounds_for_zone(at: Millis, timezone: Option<&str>, fallback_offset_min: i32) -> (Millis, Millis) {
    let Some(tz) = timezone.filter(|s| is_supported_timezone(s)) else {
        return month_bounds(at, fallback_offset_min);
    };
    let Some(off_at) = iana_offset_at(tz, at) else {
        return month_bounds(at, fallback_offset_min);
    };
    let shifted = at.0.saturating_add(i64::from(off_at) * MS_PER_MIN);
    let days = shifted.div_euclid(MS_PER_DAY);
    let (year, month, _) = civil_from_days(days);
    let (next_year, next_month) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };

    let start_utc_approx = Millis(days_from_civil(year, month, 1) * MS_PER_DAY - i64::from(off_at) * MS_PER_MIN);
    let off_start = iana_offset_at(tz, start_utc_approx).unwrap_or(off_at);
    let start_ms = days_from_civil(year, month, 1) * MS_PER_DAY - i64::from(off_start) * MS_PER_MIN;

    let end_utc_approx =
        Millis(days_from_civil(next_year, next_month, 1) * MS_PER_DAY - i64::from(off_start) * MS_PER_MIN);
    let off_end = iana_offset_at(tz, end_utc_approx).unwrap_or(off_start);
    let end_ms = days_from_civil(next_year, next_month, 1) * MS_PER_DAY - i64::from(off_end) * MS_PER_MIN;

    (Millis(start_ms), Millis(end_ms))
}

/// Computes the exact UTC offset in minutes for supported IANA zones at UTC instant `at`.
pub fn iana_offset_at(tz: &str, at: Millis) -> Option<i32> {
    if !is_supported_timezone(tz) {
        return None;
    }
    match tz {
        "UTC" => return Some(0),
        "Asia/Kolkata" => return Some(330),
        "Asia/Tokyo" => return Some(540),
        _ => {}
    }
    let approx_year = civil_from_days(at.0.div_euclid(MS_PER_DAY)).0;
    let offset = match tz {
        "Europe/London" => {
            let start = last_sunday_days(approx_year, 3) * MS_PER_DAY + MS_PER_HOUR;
            let end = last_sunday_days(approx_year, 10) * MS_PER_DAY + MS_PER_HOUR;
            if at.0 >= start && at.0 < end { 60 } else { 0 }
        }
        "Europe/Berlin" => {
            let start = last_sunday_days(approx_year, 3) * MS_PER_DAY + MS_PER_HOUR;
            let end = last_sunday_days(approx_year, 10) * MS_PER_DAY + MS_PER_HOUR;
            if at.0 >= start && at.0 < end { 120 } else { 60 }
        }
        "America/New_York" => us_dst_offset(approx_year, at.0, -300, -240),
        "America/Chicago" => us_dst_offset(approx_year, at.0, -360, -300),
        "America/Los_Angeles" => us_dst_offset(approx_year, at.0, -480, -420),
        "Australia/Sydney" => {
            // AEDT (+660) runs 1st Sun Oct 02:00 AEST -> 1st Sun Apr 03:00 AEDT across New Year.
            let end_apr = nth_sunday_days(approx_year, 4, 1) * MS_PER_DAY + 3 * MS_PER_HOUR - 660 * MS_PER_MIN;
            let start_oct = nth_sunday_days(approx_year, 10, 1) * MS_PER_DAY + 2 * MS_PER_HOUR - 600 * MS_PER_MIN;
            if at.0 < end_apr || at.0 >= start_oct { 660 } else { 600 }
        }
        _ => return None,
    };
    Some(offset)
}

fn us_dst_offset(year: i64, at_ms: i64, std_min: i32, dst_min: i32) -> i32 {
    let start = nth_sunday_days(year, 3, 2) * MS_PER_DAY + 2 * MS_PER_HOUR - i64::from(std_min) * MS_PER_MIN;
    let end = nth_sunday_days(year, 11, 1) * MS_PER_DAY + 2 * MS_PER_HOUR - i64::from(dst_min) * MS_PER_MIN;
    if at_ms >= start && at_ms < end { dst_min } else { std_min }
}

fn weekday_from_days(z: i64) -> i64 {
    (z + 4).rem_euclid(7)
}

fn nth_sunday_days(year: i64, month: u32, n: u32) -> i64 {
    let first = days_from_civil(year, month, 1);
    let wd = weekday_from_days(first);
    let first_sun_offset = (7 - wd) % 7;
    first + first_sun_offset + 7 * i64::from(n.saturating_sub(1))
}

fn last_sunday_days(year: i64, month: u32) -> i64 {
    let (ny, nm) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    let last_day = days_from_civil(ny, nm, 1) - 1;
    let wd = weekday_from_days(last_day);
    last_day - wd
}

/// Converts days since 1970-01-01 to `(year, month 1..=12, day 1..=31)` in the proleptic Gregorian
/// calendar (Howard Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = y + i64::from(m <= 2);
    (year, m, d)
}

/// Converts `(year, month 1..=12, day 1..=31)` to days since 1970-01-01.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let m64 = u64::from(m);
    let doy = (153 * (if m64 > 2 { m64 - 3 } else { m64 + 9 }) + 2) / 5 + u64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + (doe as i64) - 719_468
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TipRecord {
    source_id: Option<String>,
    actor_key: String,
    actor_name: String,
    anonymous: bool,
    money: Money,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct SupporterStats {
    name: String,
    subs: u64,
    bits: u64,
    tips: Vec<Money>,
    raids: u64,
}

impl SupporterStats {
    /// Deterministic local contribution score for ranking (subs ≈ 500 pts, 1 bit = 1 pt,
    /// tip normalized via coarse USD cents). Never displayed as converted currency!
    fn score(&self) -> u128 {
        let mut s = u128::from(self.subs) * 500 + u128::from(self.bits);
        for m in &self.tips {
            let cents_equiv = u128::try_from(normalized_cents(m.minor.max(0), &m.currency).max(0)).unwrap_or(0);
            s = s.saturating_add(cents_equiv);
        }
        s
    }

    fn summary(&self) -> String {
        let mut parts = Vec::new();
        for m in &self.tips {
            if m.minor > 0 {
                parts.push(m.display());
            }
        }
        if self.subs > 0 {
            let noun = if self.subs == 1 { "sub" } else { "subs" };
            parts.push(format!("{} {noun}", self.subs));
        }
        if self.bits > 0 {
            parts.push(format!("{} bits", format_thousands(self.bits)));
        }
        if parts.is_empty() { self.name.clone() } else { format!("{} ({})", self.name, parts.join(", ")) }
    }
}

/// Aggregated label totals and leaderboards across any period (session, month, or all-time).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeriodTotals {
    pub follows: u64,
    pub subs: u64,
    pub bits: u64,
    pub raids: u64,
    pub raid_viewers: u64,
    tips_log: Vec<TipRecord>,
    reversed: HashSet<String>,
    supporters: HashMap<String, SupporterStats>,
    #[serde(default)]
    next_anon_id: u64,
}

impl PeriodTotals {
    pub fn observe(&mut self, event: &Event) {
        if event.origin != Origin::Live {
            return;
        }
        match &event.kind {
            EventKind::Reversal { original_source_id, .. } => {
                self.reverse(original_source_id);
            }
            EventKind::Follow { .. } => {
                self.follows = self.follows.saturating_add(1);
            }
            EventKind::Subscription { actor, gifted_by: None, .. } => {
                self.subs = self.subs.saturating_add(1);
                if !actor.anonymous {
                    let k = actor_key(event.platform, actor.id.as_deref(), &actor.display_name);
                    let entry = self
                        .supporters
                        .entry(k)
                        .or_insert_with(|| SupporterStats { name: actor.display_name.clone(), ..Default::default() });
                    entry.name = actor.display_name.clone();
                    entry.subs = entry.subs.saturating_add(1);
                }
            }
            EventKind::GiftBatch { gifter, count, .. } => {
                let c = u64::from(*count);
                self.subs = self.subs.saturating_add(c);
                if !gifter.anonymous {
                    let k = actor_key(event.platform, gifter.id.as_deref(), &gifter.display_name);
                    let entry = self
                        .supporters
                        .entry(k)
                        .or_insert_with(|| SupporterStats { name: gifter.display_name.clone(), ..Default::default() });
                    entry.name = gifter.display_name.clone();
                    entry.subs = entry.subs.saturating_add(c);
                }
            }
            EventKind::Tip { actor, money, status: PaymentStatus::Captured, .. }
            | EventKind::PaidMessage { actor, money, .. } => {
                if money.minor <= 0 {
                    return;
                }
                if let Some(src) = &event.source_id
                    && self.reversed.contains(src)
                {
                    return;
                }
                let k = if actor.anonymous {
                    let id = self.next_anon_id;
                    self.next_anon_id = self.next_anon_id.saturating_add(1);
                    format!("anon:{id}")
                } else {
                    actor_key(event.platform, actor.id.as_deref(), &actor.display_name)
                };
                let name = if actor.anonymous { "Anonymous".to_string() } else { actor.display_name.clone() };
                self.tips_log.push(TipRecord {
                    source_id: event.source_id.clone(),
                    actor_key: k.clone(),
                    actor_name: name.clone(),
                    anonymous: actor.anonymous,
                    money: money.clone(),
                });
                if !actor.anonymous {
                    let entry = self
                        .supporters
                        .entry(k)
                        .or_insert_with(|| SupporterStats { name: name.clone(), ..Default::default() });
                    entry.name = name;
                    add_money(&mut entry.tips, money);
                }
            }
            EventKind::PlatformGift { actor, value, .. } if value.unit == ValueUnit::Bits => {
                self.bits = self.bits.saturating_add(value.amount);
                if !actor.anonymous {
                    let k = actor_key(event.platform, actor.id.as_deref(), &actor.display_name);
                    let entry = self
                        .supporters
                        .entry(k)
                        .or_insert_with(|| SupporterStats { name: actor.display_name.clone(), ..Default::default() });
                    entry.name = actor.display_name.clone();
                    entry.bits = entry.bits.saturating_add(value.amount);
                }
            }
            EventKind::Raid { from, viewers } => {
                self.raids = self.raids.saturating_add(1);
                self.raid_viewers = self.raid_viewers.saturating_add(u64::from(*viewers));
                let k = actor_key(event.platform, from.id.as_deref(), &from.display_name);
                let entry = self
                    .supporters
                    .entry(k)
                    .or_insert_with(|| SupporterStats { name: from.display_name.clone(), ..Default::default() });
                entry.name = from.display_name.clone();
                entry.raids = entry.raids.saturating_add(1);
            }
            _ => {}
        }
    }

    pub fn reverse(&mut self, original_source_id: &str) {
        self.reversed.insert(original_source_id.to_string());
        let mut removed = Vec::new();
        self.tips_log.retain(|t| {
            if t.source_id.as_deref() == Some(original_source_id) {
                removed.push((t.actor_key.clone(), t.money.clone(), t.anonymous));
                false
            } else {
                true
            }
        });
        for (k, money, anon) in removed {
            if !anon && let Some(s) = self.supporters.get_mut(&k) {
                sub_money(&mut s.tips, &money);
            }
        }
    }

    /// Total tips per currency in order seen (never auto-converted).
    pub fn tips_by_currency(&self) -> Vec<Money> {
        let mut totals = Vec::new();
        for t in &self.tips_log {
            add_money(&mut totals, &t.money);
        }
        totals
    }

    pub fn formatted_tips(&self) -> String {
        let totals = self.tips_by_currency();
        if totals.is_empty() { "0".into() } else { totals.iter().map(Money::display).collect::<Vec<_>>().join(" · ") }
    }

    /// Cumulative top tipper per currency or largest tip amount (returns `"{name} {amount}"`).
    pub fn top_tip_display(&self) -> String {
        // Group cumulative tip amounts per named donor + currency (falling back to single tips if needed)
        let mut by_donor: BTreeMap<(String, String), (String, i64)> = BTreeMap::new();
        for t in &self.tips_log {
            let entry = by_donor
                .entry((t.actor_key.clone(), t.money.currency.clone()))
                .or_insert_with(|| (t.actor_name.clone(), 0));
            entry.1 = entry.1.saturating_add(t.money.minor);
        }
        let best = by_donor.into_iter().max_by(|((_, c_a), (n_a, m_a)), ((_, c_b), (n_b, m_b))| {
            let s_a = normalized_cents(*m_a, c_a);
            let s_b = normalized_cents(*m_b, c_b);
            s_a.cmp(&s_b).then_with(|| n_b.cmp(n_a))
        });
        best.map(|((_, currency), (name, minor))| format!("{name} {}", Money::new(minor, &currency).display()))
            .unwrap_or_default()
    }

    pub fn top_supporter_display(&self) -> String {
        let best = self.supporters.values().filter(|s| s.score() > 0).max_by(|a, b| {
            a.score()
                .cmp(&b.score())
                .then_with(|| a.subs.cmp(&b.subs))
                .then_with(|| a.bits.cmp(&b.bits))
                .then_with(|| b.name.cmp(&a.name))
        });
        best.map(SupporterStats::summary).unwrap_or_default()
    }
}

/// Latest-X snapshots across all streams (hydrated from SQLite and updated live).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LatestLabels {
    pub follower: Option<String>,
    pub sub: Option<String>,
    pub gifter: Option<(String, u32)>,
    pub tip: Option<(String, Money)>,
    pub bits: Option<(String, u64)>,
    pub raid: Option<(String, u32)>,
    #[serde(skip)]
    tip_history: Vec<(Option<String>, String, Money)>,
    #[serde(skip)]
    reversed_sources: HashSet<String>,
}

impl PartialEq for LatestLabels {
    fn eq(&self, other: &Self) -> bool {
        self.follower == other.follower
            && self.sub == other.sub
            && self.gifter == other.gifter
            && self.tip == other.tip
            && self.bits == other.bits
            && self.raid == other.raid
    }
}

impl Eq for LatestLabels {}

impl LatestLabels {
    pub fn observe(&mut self, event: &Event) {
        if event.origin != Origin::Live {
            return;
        }
        match &event.kind {
            EventKind::Reversal { original_source_id, .. } => {
                self.reversed_sources.insert(original_source_id.clone());
                self.tip_history.retain(|(src, _, _)| src.as_deref() != Some(original_source_id.as_str()));
                self.tip = self.tip_history.last().map(|(_, n, m)| (n.clone(), m.clone()));
            }
            EventKind::Follow { actor } => {
                self.follower = Some(actor.display_name.clone());
            }
            EventKind::Subscription { actor, gifted_by: None, .. } => {
                self.sub = Some(actor.display_name.clone());
            }
            EventKind::GiftBatch { gifter, count, .. } => {
                self.gifter = Some((gifter.display_name.clone(), *count));
            }
            EventKind::Tip { actor, money, status: PaymentStatus::Captured, .. }
            | EventKind::PaidMessage { actor, money, .. } => {
                if money.minor <= 0 {
                    return;
                }
                if let Some(src) = &event.source_id
                    && self.reversed_sources.contains(src)
                {
                    return;
                }
                let name = if actor.anonymous { "Anonymous".to_string() } else { actor.display_name.clone() };
                self.tip_history.push((event.source_id.clone(), name.clone(), money.clone()));
                if self.tip_history.len() > 32 {
                    self.tip_history.remove(0);
                }
                self.tip = Some((name, money.clone()));
            }
            EventKind::PlatformGift { actor, value, .. } if value.unit == ValueUnit::Bits => {
                self.bits = Some((actor.display_name.clone(), value.amount));
            }
            EventKind::Raid { from, viewers } => {
                self.raid = Some((from.display_name.clone(), *viewers));
            }
            _ => {}
        }
    }

    pub fn label(&self, source: LabelSource) -> Option<String> {
        match source {
            LabelSource::LatestFollower => Some(self.follower.clone().unwrap_or_default()),
            LabelSource::LatestSub => Some(self.sub.clone().unwrap_or_default()),
            LabelSource::LatestGifter => {
                Some(self.gifter.as_ref().map(|(n, c)| format!("{n} ({c})")).unwrap_or_default())
            }
            LabelSource::LatestTip => {
                Some(self.tip.as_ref().map(|(n, m)| format!("{n} {}", m.display())).unwrap_or_default())
            }
            LabelSource::LatestBits => {
                Some(self.bits.as_ref().map(|(n, b)| format!("{n} {} bits", format_thousands(*b))).unwrap_or_default())
            }
            LabelSource::LatestRaid => Some(self.raid.as_ref().map(|(n, v)| format!("{n} ({v})")).unwrap_or_default()),
            _ => None,
        }
    }
}

/// Evaluates `PeriodTotals` over a slice of chronological events.
pub fn summarize_events(events: &[Event]) -> PeriodTotals {
    let mut totals = PeriodTotals::default();
    for e in events {
        totals.observe(e);
    }
    totals
}

// ========================================================================= Combo / Hype Train

/// Timing and trigger settings for a combo / hype train.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComboTrainConfig {
    /// Initial window in seconds (`10..=3600`). Warm-up events must arrive within this span, and
    /// an activated train starts with this countdown duration.
    pub window_s: u32,
    /// Seconds added to the remaining countdown per qualifying event within an active train (`1..=600`).
    pub step_s: u32,
    /// Maximum remaining countdown seconds allowed when extending (`10..=7200`, `>= window_s`).
    pub max_s: u32,
    /// Qualifying live events needed inside `window_s` to ignite the train (`1..=50`).
    pub threshold: u32,
}

impl Default for ComboTrainConfig {
    fn default() -> Self {
        Self { window_s: 120, step_s: 15, max_s: 300, threshold: 3 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrainPulse {
    at: Millis,
    actor_key: String,
    actor_name: String,
    source_id: Option<String>,
}

/// Serialized hype/combo train state pushed to overlays (`state.label_trains[widget_id]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComboTrainView {
    pub active: bool,
    pub level: u32,
    pub events: u32,
    pub contributors: u32,
    pub latest_actor: Option<String>,
    pub remaining_ms: u64,
    pub ends_at: Option<Millis>,
    pub warmup_count: u32,
    pub threshold: u32,
}

/// Pure hype/combo train state machine.
#[derive(Debug, Clone)]
pub struct ComboTrain {
    cfg: ComboTrainConfig,
    warmup: VecDeque<TrainPulse>,
    active: Option<ActiveTrain>,
    reversed: HashSet<String>,
}

#[derive(Debug, Clone)]
struct ActiveTrain {
    ends_at: Millis,
    events: Vec<TrainPulse>,
}

impl ComboTrain {
    pub fn new(cfg: ComboTrainConfig) -> Self {
        Self { cfg, warmup: VecDeque::new(), active: None, reversed: HashSet::new() }
    }

    pub fn config(&self) -> ComboTrainConfig {
        self.cfg
    }

    fn window_ms(&self) -> i64 {
        i64::from(self.cfg.window_s.max(1)) * 1000
    }

    fn step_ms(&self) -> i64 {
        i64::from(self.cfg.step_s.max(1)) * 1000
    }

    fn max_ms(&self) -> i64 {
        i64::from(self.cfg.max_s.max(self.cfg.window_s)) * 1000
    }

    /// Expires active train or stale warm-up events as of `now` (`ends_at <= now` is expired).
    /// Returns `true` if an active train expired during this call.
    pub fn tick(&mut self, now: Millis) -> bool {
        let mut expired = false;
        if let Some(a) = &self.active
            && now.0 >= a.ends_at.0
        {
            self.active = None;
            self.warmup.clear();
            expired = true;
        }
        // Every stale pulse goes, wherever it sits: pulses are stamped with the arrival clock but
        // a late one can sit behind a newer one, and an expired pulse must not count.
        let w_ms = self.window_ms();
        self.warmup.retain(|p| now.0.saturating_sub(p.at.0) < w_ms);
        expired
    }

    /// Processes a single incoming event using `event.occurred_at` as the clock.
    pub fn observe(&mut self, event: &Event) {
        self.observe_at(event, event.occurred_at);
    }

    /// Processes a single incoming event anchored at caller-supplied monotonic wall clock `now`.
    pub fn observe_at(&mut self, event: &Event, now: Millis) {
        if event.origin != Origin::Live {
            return;
        }
        if let EventKind::Reversal { original_source_id, .. } = &event.kind {
            self.reverse(original_source_id, now);
            return;
        }
        // Ignore obviously historical/backdated events (>1h older than local clock) while
        // tolerating normal platform-vs-PC clock skew so real-time arrivals anchor to `now`.
        const MAX_TRAIN_CLOCK_SKEW_MS: i64 = 3_600_000;
        if now.0.saturating_sub(event.occurred_at.0) > MAX_TRAIN_CLOCK_SKEW_MS {
            return;
        }
        let Some((actor_key, actor_name)) = train_actor(event) else {
            return;
        };
        if let Some(src) = &event.source_id
            && self.reversed.contains(src)
        {
            return;
        }
        self.tick(now);

        let pulse = TrainPulse { at: now, actor_key, actor_name, source_id: event.source_id.clone() };

        let step_ms = self.step_ms();
        let max_ms = self.max_ms();
        if let Some(a) = &mut self.active {
            // Inside active window: extend countdown by `step_ms`, capped at `now + max_ms`,
            // while never shrinking an existing deadline on out-of-order events.
            let extended = a.ends_at.0.saturating_add(step_ms);
            let capped = now.0.saturating_add(max_ms);
            a.ends_at = Millis(a.ends_at.0.max(extended.min(capped)));
            a.events.push(pulse);
            return;
        }

        self.warmup.push_back(pulse);
        if self.warmup.len() >= usize::try_from(self.cfg.threshold.max(1)).unwrap_or(1) {
            let events: Vec<TrainPulse> = self.warmup.drain(..).collect();
            self.active = Some(ActiveTrain { ends_at: Millis(now.0.saturating_add(self.window_ms())), events });
        }
    }

    /// Removes a reversed tip from warm-up / active event counts safely.
    pub fn reverse(&mut self, original_source_id: &str, now: Millis) {
        self.reversed.insert(original_source_id.to_string());
        self.tick(now);
        self.warmup.retain(|p| p.source_id.as_deref() != Some(original_source_id));
        let threshold = usize::try_from(self.cfg.threshold.max(1)).unwrap_or(1);
        if let Some(a) = &mut self.active {
            a.events.retain(|p| p.source_id.as_deref() != Some(original_source_id));
            if a.events.len() < threshold {
                let remaining = std::mem::take(&mut a.events);
                self.active = None;
                self.warmup = remaining.into_iter().collect();
            }
        }
    }

    pub fn view(&self, now: Millis) -> ComboTrainView {
        if let Some(a) = &self.active
            && now.0 < a.ends_at.0
        {
            let count = u32::try_from(a.events.len()).unwrap_or(u32::MAX);
            let contributors = a.events.iter().map(|p| p.actor_key.as_str()).collect::<HashSet<_>>().len();
            let extra = count.saturating_sub(self.cfg.threshold);
            let level = 1 + extra / 5;
            let remaining_ms = u64::try_from(a.ends_at.0 - now.0).unwrap_or(0);
            return ComboTrainView {
                active: true,
                level,
                events: count,
                contributors: u32::try_from(contributors).unwrap_or(u32::MAX),
                latest_actor: a.events.last().map(|p| p.actor_name.clone()),
                remaining_ms,
                ends_at: Some(a.ends_at),
                warmup_count: count,
                threshold: self.cfg.threshold,
            };
        }
        let w_ms = self.window_ms();
        let active_warmup =
            self.warmup.iter().filter(|p| now.0 >= p.at.0 && now.0.saturating_sub(p.at.0) < w_ms).count();
        ComboTrainView {
            active: false,
            level: 0,
            events: 0,
            contributors: 0,
            latest_actor: None,
            remaining_ms: 0,
            ends_at: None,
            warmup_count: u32::try_from(active_warmup).unwrap_or(0),
            threshold: self.cfg.threshold,
        }
    }

    pub fn render(&self, template: &str, now: Millis) -> String {
        let v = self.view(now);
        if !v.active {
            return String::new();
        }
        let timer = format_timer(v.remaining_ms);
        let default_summary = format!("Lv.{} · {} events · {}", v.level, v.events, timer);
        let mut vars = Vars::new();
        vars.insert("value".into(), default_summary.clone());
        vars.insert("name".into(), default_summary);
        vars.insert("level".into(), v.level.to_string());
        vars.insert("count".into(), v.events.to_string());
        vars.insert("events".into(), v.events.to_string());
        vars.insert("contributors".into(), v.contributors.to_string());
        vars.insert("timer".into(), timer);
        vars.insert("latest".into(), v.latest_actor.unwrap_or_default());
        Template::parse(template).map(|t| t.render(&vars)).unwrap_or_default()
    }
}

fn train_actor(event: &Event) -> Option<(String, String)> {
    match &event.kind {
        EventKind::Follow { actor } => {
            Some((actor_key(event.platform, actor.id.as_deref(), &actor.display_name), actor.display_name.clone()))
        }
        EventKind::Subscription { actor, gifted_by: None, .. } => {
            Some((actor_key(event.platform, actor.id.as_deref(), &actor.display_name), actor.display_name.clone()))
        }
        EventKind::GiftBatch { gifter, .. } => {
            Some((actor_key(event.platform, gifter.id.as_deref(), &gifter.display_name), gifter.display_name.clone()))
        }
        EventKind::Tip { actor, money, status: PaymentStatus::Captured, .. }
        | EventKind::PaidMessage { actor, money, .. }
            if money.minor > 0 =>
        {
            let name = if actor.anonymous { "Anonymous".to_string() } else { actor.display_name.clone() };
            Some((actor_key(event.platform, actor.id.as_deref(), &name), name))
        }
        EventKind::PlatformGift { actor, .. } => {
            Some((actor_key(event.platform, actor.id.as_deref(), &actor.display_name), actor.display_name.clone()))
        }
        EventKind::Raid { from, .. } => {
            Some((actor_key(event.platform, from.id.as_deref(), &from.display_name), from.display_name.clone()))
        }
        _ => None,
    }
}

pub fn format_timer(remaining_ms: u64) -> String {
    let total_secs = remaining_ms.div_ceil(1000);
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    if hours > 0 { format!("{hours}:{mins:02}:{secs:02}") } else { format!("{mins:02}:{secs:02}") }
}

pub fn format_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn actor_key(platform: Platform, id: Option<&str>, name: &str) -> String {
    match id {
        Some(id) if !id.is_empty() => format!("{}:{id}", platform_slug(platform)),
        _ => format!("{}:name:{}", platform_slug(platform), name.to_lowercase()),
    }
}

fn platform_slug(p: Platform) -> &'static str {
    match p {
        Platform::Twitch => "twitch",
        Platform::Youtube => "youtube",
        Platform::Kick => "kick",
        Platform::Tips => "tips",
        Platform::Ferrocast => "ferrocast",
    }
}

fn add_money(list: &mut Vec<Money>, m: &Money) {
    match list.iter_mut().find(|x| x.currency == m.currency) {
        Some(x) => x.minor = x.minor.saturating_add(m.minor),
        None => list.push(m.clone()),
    }
}

fn sub_money(list: &mut Vec<Money>, m: &Money) {
    if let Some(pos) = list.iter().position(|x| x.currency == m.currency) {
        list[pos].minor = list[pos].minor.saturating_sub(m.minor);
        if list[pos].minor <= 0 {
            list.remove(pos);
        }
    }
}

/// Coarse USD-cents equivalent purely for cross-currency ranking (`Top tip` / `Top supporter`).
/// Rendered currency totals and single tips always stay in their original currency without FX conversion.
fn normalized_cents(minor: i64, currency: &str) -> i128 {
    let scale = 10_i128.pow(exponent(currency));
    let milli_cents_per_major = match currency {
        "USD" | "USDC" | "USDT" => 100_000,
        "EUR" => 108_000,
        "GBP" => 127_000,
        "CHF" => 113_000,
        "CAD" => 72_000,
        "AUD" => 65_000,
        "NZD" => 60_000,
        "SGD" => 74_000,
        "HKD" => 12_800,
        "INR" => 1_180,
        "JPY" => 650,
        "KRW" => 72,
        "CNY" => 13_800,
        "TWD" => 3_100,
        "BRL" => 17_500,
        "MXN" => 5_000,
        "PHP" => 1_720,
        "TRY" => 2_900,
        "PLN" => 24_500,
        "SEK" => 9_200,
        "NOK" => 9_100,
        "DKK" => 14_500,
        "ZAR" => 5_500,
        "SAR" => 26_700,
        "AED" => 27_200,
        "VND" => 4,
        "IDR" => 6,
        "CLP" => 105,
        "ISK" => 720,
        "BHD" => 265_000,
        "KWD" => 325_000,
        "OMR" => 260_000,
        "JOD" => 141_000,
        other if exponent(other) == 0 => 100,
        _ => 100_000,
    };
    if scale > 0 { i128::from(minor) * milli_cents_per_major / (1_000 * scale) } else { i128::from(minor) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Actor, Content, ReversalReason, Tier, TipProvider};
    use crate::money::PlatformValue;

    fn utc_ms(year: i64, month: u32, day: u32, hour: i64, min: i64, sec: i64, ms: i64) -> Millis {
        let d = days_from_civil(year, month, day);
        Millis(d * MS_PER_DAY + (hour * 3600 + min * 60 + sec) * 1000 + ms)
    }

    fn who(name: &str) -> Actor {
        let mut a = Actor::named(Platform::Twitch, name);
        a.id = Some(format!("id-{name}"));
        a
    }

    fn follow(at: i64, name: &str) -> Event {
        Event::live(
            Platform::Twitch,
            Some(format!("f-{at}-{name}")),
            Millis(at),
            EventKind::Follow { actor: who(name) },
        )
    }

    fn sub(at: i64, name: &str) -> Event {
        Event::live(
            Platform::Twitch,
            Some(format!("s-{at}-{name}")),
            Millis(at),
            EventKind::Subscription { actor: who(name), tier: Tier::Tier1, months: 1, gifted_by: None, message: None },
        )
    }

    fn gift(at: i64, gifter: &str, count: u32) -> Event {
        Event::live(
            Platform::Twitch,
            Some(format!("g-{at}-{gifter}")),
            Millis(at),
            EventKind::GiftBatch { gifter: who(gifter), tier: Tier::Tier1, count },
        )
    }

    fn tip(at: i64, id: &str, name: &str, minor: i64, currency: &str, status: PaymentStatus) -> Event {
        Event::live(
            Platform::Tips,
            Some(id.into()),
            Millis(at),
            EventKind::Tip {
                actor: who(name),
                money: Money::new(minor, currency),
                message: None,
                provider: TipProvider::Razorpay,
                payment_ref: id.into(),
                status,
            },
        )
    }

    fn bits(at: i64, name: &str, amount: u64) -> Event {
        Event::live(
            Platform::Twitch,
            Some(format!("b-{at}-{name}")),
            Millis(at),
            EventKind::PlatformGift {
                actor: who(name),
                value: PlatformValue { unit: ValueUnit::Bits, amount },
                message: None,
            },
        )
    }

    fn raid(at: i64, name: &str, viewers: u32) -> Event {
        Event::live(
            Platform::Twitch,
            Some(format!("r-{at}-{name}")),
            Millis(at),
            EventKind::Raid { from: who(name), viewers },
        )
    }

    #[test]
    fn month_boundaries_handle_leap_years_year_rollover_and_tz_offsets() {
        // Non-leap Feb 2025 -> Mar 2025 in UTC
        let mid_feb = utc_ms(2025, 2, 15, 12, 0, 0, 0);
        assert_eq!(month_bounds(mid_feb, 0), (utc_ms(2025, 2, 1, 0, 0, 0, 0), utc_ms(2025, 3, 1, 0, 0, 0, 0)));

        // Leap Feb 2024 includes Feb 29 23:59:59.999
        let leap_day = utc_ms(2024, 2, 29, 23, 59, 59, 999);
        assert_eq!(month_bounds(leap_day, 0), (utc_ms(2024, 2, 1, 0, 0, 0, 0), utc_ms(2024, 3, 1, 0, 0, 0, 0)));
        // Exact boundary 2024-03-01T00:00:00.000Z moves to March
        let mar_start = utc_ms(2024, 3, 1, 0, 0, 0, 0);
        assert_eq!(month_bounds(mar_start, 0), (mar_start, utc_ms(2024, 4, 1, 0, 0, 0, 0)));

        // Dec -> Jan rollover
        let dec_last = utc_ms(2025, 12, 31, 23, 59, 59, 999);
        assert_eq!(month_bounds(dec_last, 0), (utc_ms(2025, 12, 1, 0, 0, 0, 0), utc_ms(2026, 1, 1, 0, 0, 0, 0)));

        // Positive TZ offset (+05:30 = +330 min): 2026-01-31T18:30:00Z IS 2026-02-01 00:00:00+05:30!
        let ist_feb_1_instant = utc_ms(2026, 1, 31, 18, 30, 0, 0);
        let (s, e) = month_bounds(ist_feb_1_instant, 330);
        assert_eq!(s, ist_feb_1_instant);
        assert_eq!(e, utc_ms(2026, 2, 28, 18, 30, 0, 0));

        // DST-aware Europe/London: March 29 2026 shifts GMT (+0) -> BST (+60), so March 31 23:30 UTC is April 1 BST
        let lon_mar_15 = utc_ms(2026, 3, 15, 12, 0, 0, 0);
        assert_eq!(
            month_bounds_for_zone(lon_mar_15, Some("Europe/London"), 0),
            (utc_ms(2026, 3, 1, 0, 0, 0, 0), utc_ms(2026, 3, 31, 23, 0, 0, 0))
        );
        let lon_apr_start = utc_ms(2026, 3, 31, 23, 30, 0, 0);
        assert_eq!(
            month_bounds_for_zone(lon_apr_start, Some("Europe/London"), 0),
            (utc_ms(2026, 3, 31, 23, 0, 0, 0), utc_ms(2026, 4, 30, 23, 0, 0, 0))
        );

        // DST-aware America/New_York: March 8 2026 shifts EST (-300) -> EDT (-240)
        let ny_mar_15 = utc_ms(2026, 3, 15, 12, 0, 0, 0);
        assert_eq!(
            month_bounds_for_zone(ny_mar_15, Some("America/New_York"), 0),
            (utc_ms(2026, 3, 1, 5, 0, 0, 0), utc_ms(2026, 4, 1, 4, 0, 0, 0))
        );
        // Southern-hemisphere Australia/Sydney: Jan is AEDT (+660), July is AEST (+600)
        assert_eq!(iana_offset_at("Australia/Sydney", utc_ms(2026, 1, 15, 0, 0, 0, 0)), Some(660));
        assert_eq!(iana_offset_at("Australia/Sydney", utc_ms(2026, 7, 15, 0, 0, 0, 0)), Some(600));
    }

    #[test]
    fn label_maths_across_session_month_boundaries_currencies_and_refunds() {
        let mut session = PeriodTotals::default();
        let mut monthly = PeriodTotals::default();
        let mut alltime = PeriodTotals::default();
        let mut latest = LatestLabels::default();

        // Events in previous month (Jan 31) -> only in all-time
        let jan_tip =
            tip(utc_ms(2026, 1, 31, 23, 0, 0, 0).0, "t-jan", "OldPatron", 100_000, "INR", PaymentStatus::Captured);
        // Earlier stream this month (Feb 10) -> in monthly & all-time
        let feb_sub = gift(utc_ms(2026, 2, 10, 18, 0, 0, 0).0, "FebGifter", 4);
        // Test & pending events -> ignored everywhere
        let mut test_follow = follow(utc_ms(2026, 2, 20, 10, 0, 0, 0).0, "FakeTest");
        test_follow.origin = Origin::Test;
        let pending_tip =
            tip(utc_ms(2026, 2, 20, 10, 1, 0, 0).0, "t-pend", "Pend", 50_000, "USD", PaymentStatus::Pending);

        for e in [&jan_tip, &feb_sub, &test_follow, &pending_tip] {
            alltime.observe(e);
            latest.observe(e);
        }
        monthly.observe(&feb_sub);
        monthly.observe(&test_follow);
        monthly.observe(&pending_tip);

        // Current session starts Feb 20 (monthly already has 4 subs)
        let ses_events = vec![
            follow(utc_ms(2026, 2, 20, 11, 0, 0, 0).0, "Alice"),
            sub(utc_ms(2026, 2, 20, 11, 1, 0, 0).0, "Bob"),
            bits(utc_ms(2026, 2, 20, 11, 2, 0, 0).0, "Carol", 1500),
            raid(utc_ms(2026, 2, 20, 11, 3, 0, 0).0, "Dan", 42),
            tip(utc_ms(2026, 2, 20, 11, 4, 0, 0).0, "t1", "Eve", 50_000, "INR", PaymentStatus::Captured),
            tip(utc_ms(2026, 2, 20, 11, 5, 0, 0).0, "t2", "Frank", 60_000, "USD", PaymentStatus::Captured),
        ];
        for e in &ses_events {
            session.observe(e);
            monthly.observe(e);
            alltime.observe(e);
            latest.observe(e);
        }

        assert_eq!(session.follows, 1);
        assert_eq!(session.subs, 1);
        assert_eq!(session.bits, 1500);
        assert_eq!(session.raids, 1);
        assert_eq!(session.formatted_tips(), "₹500 · $600");
        assert_eq!(monthly.subs, 5);
        assert_eq!(monthly.formatted_tips(), "₹500 · $600");
        assert_eq!(alltime.formatted_tips(), "₹1,500 · $600");
        assert!(alltime.top_supporter_display().starts_with("Frank"));
        assert!(session.top_supporter_display().starts_with("Frank"));

        // Refund Frank's $600 tip (`t2`) -> drops Frank from top supporter & removes USD
        let rev = Event::live(
            Platform::Tips,
            Some("rev-t2".into()),
            Millis(utc_ms(2026, 2, 20, 11, 6, 0, 0).0),
            EventKind::Reversal { original_source_id: "t2".into(), reason: ReversalReason::Refund },
        );
        session.observe(&rev);
        monthly.observe(&rev);
        alltime.observe(&rev);
        latest.observe(&rev);

        assert_eq!(session.formatted_tips(), "₹500");
        assert!(session.top_supporter_display().starts_with("Carol"));
        assert_eq!(alltime.formatted_tips(), "₹1,500");

        // Latest labels
        assert_eq!(latest.label(LabelSource::LatestFollower).as_deref(), Some("Alice"));
        assert_eq!(latest.label(LabelSource::LatestSub).as_deref(), Some("Bob"));
        assert_eq!(latest.label(LabelSource::LatestGifter).as_deref(), Some("FebGifter (4)"));
        assert_eq!(latest.label(LabelSource::LatestBits).as_deref(), Some("Carol 1,500 bits"));
        assert_eq!(latest.label(LabelSource::LatestRaid).as_deref(), Some("Dan (42)"));
        assert_eq!(latest.label(LabelSource::LatestTip).as_deref(), Some("Eve ₹500"));
    }

    #[test]
    fn hype_train_handles_warmup_window_edges_growth_cap_expiry_and_test_isolation() {
        let cfg = ComboTrainConfig { window_s: 60, step_s: 15, max_s: 90, threshold: 3 };
        let mut train = ComboTrain::new(cfg);

        // Ignored chat & test events
        let mut t_ev = follow(0, "TestFan");
        t_ev.origin = Origin::Test;
        train.observe(&t_ev);
        let chat_ev = Event::live(
            Platform::Twitch,
            Some("c1".into()),
            Millis(100),
            EventKind::ChatMessage {
                actor: who("Chatter"),
                content: Content::plain("hype"),
                reply_to: None,
                is_action: false,
            },
        );
        train.observe(&chat_ev);
        assert!(!train.view(Millis(100)).active);
        assert_eq!(train.view(Millis(100)).warmup_count, 0);

        // Exact warm-up expiry boundary: event at 0, next at 60_000 (== 0 + window_ms) evicts event@0!
        train.observe(&follow(0, "A"));
        assert_eq!(train.view(Millis(59_999)).warmup_count, 1);
        assert_eq!(train.view(Millis(60_000)).warmup_count, 0);
        train.observe(&follow(60_000, "B"));
        assert_eq!(train.view(Millis(60_000)).warmup_count, 1);

        // Two more events within window ignite at t = 90_000 (since 60_000 + 60_000 = 120_000 > 90_000)
        train.observe(&sub(70_000, "C"));
        assert!(!train.view(Millis(70_000)).active);
        train.observe(&bits(90_000, "D", 100));
        let v = train.view(Millis(90_000));
        assert!(v.active);
        assert_eq!(v.events, 3);
        assert_eq!(v.contributors, 3);
        assert_eq!(v.level, 1);
        assert_eq!(v.ends_at, Some(Millis(150_000)));
        assert_eq!(v.remaining_ms, 60_000);

        // Event inside active window grows countdown by 15s (at 100_000: 50s left + 15s = 65s -> ends_at 165_000)
        train.observe(&follow(100_000, "E"));
        let v2 = train.view(Millis(100_000));
        assert_eq!(v2.ends_at, Some(Millis(165_000)));
        assert_eq!(v2.remaining_ms, 65_000);
        assert_eq!(v2.events, 4);

        // Repeated events grow up to cap max_s=90s (100_000 + 90_000 = 190_000)
        train.observe(&follow(100_000, "F")); // 180_000
        train.observe(&follow(100_000, "G")); // capped at 190_000, level 2 (6 events -> extra=3? No 8 hits level 2)
        assert_eq!(train.view(Millis(100_000)).ends_at, Some(Millis(190_000)));
        assert_eq!(train.view(Millis(100_000)).remaining_ms, 90_000);

        // Reach Level 2 at 8 events (threshold 3 + 5 = 8)
        train.observe(&follow(100_000, "H"));
        train.observe(&follow(100_000, "I"));
        let v3 = train.view(Millis(100_000));
        assert_eq!(v3.level, 2);
        assert_eq!(v3.events, 8);
        assert_eq!(train.render("Hype! {value}", Millis(100_000)), "Hype! Lv.2 · 8 events · 01:30");

        // Exact expiry edge: 189_999 still active (1ms left); 190_000 expired -> renders ""
        assert!(train.view(Millis(189_999)).active);
        assert!(!train.tick(Millis(189_999)));
        assert!(!train.view(Millis(190_000)).active);
        assert!(train.tick(Millis(190_000)));
        assert_eq!(train.render("{value}", Millis(190_000)), "");
    }

    #[test]
    fn gifted_subscription_recipients_are_not_double_counted() {
        let mut period = PeriodTotals::default();
        let mut latest = LatestLabels::default();
        let mut train = ComboTrain::new(ComboTrainConfig { window_s: 60, step_s: 15, max_s: 90, threshold: 2 });

        let own_sub = sub(1_000, "RealSub");
        let batch = Event::live(
            Platform::Twitch,
            Some("gb1".into()),
            Millis(2_000),
            EventKind::GiftBatch { gifter: who("GenerousGifter"), tier: Tier::Tier1, count: 5 },
        );
        let gifted_sub = Event::live(
            Platform::Twitch,
            Some("gs1".into()),
            Millis(2_100),
            EventKind::Subscription {
                actor: who("LuckyRecipient"),
                tier: Tier::Tier1,
                months: 1,
                gifted_by: Some(who("GenerousGifter")),
                message: None,
            },
        );

        for ev in [&own_sub, &batch, &gifted_sub] {
            period.observe(ev);
            latest.observe(ev);
            train.observe(ev);
        }

        // 1 own sub + 5 from GiftBatch = 6 total (not 7!)
        assert_eq!(period.subs, 6);
        assert!(!period.supporters.values().any(|s| s.name == "LuckyRecipient"));
        assert_eq!(latest.label(LabelSource::LatestSub).as_deref(), Some("RealSub"));
        assert_eq!(latest.label(LabelSource::LatestGifter).as_deref(), Some("GenerousGifter (5)"));

        // Train only counts own_sub + batch = 2 events (not 3!)
        let v = train.view(Millis(2_100));
        assert!(v.active);
        assert_eq!(v.events, 2);
        assert_eq!(v.latest_actor.as_deref(), Some("GenerousGifter"));
    }

    #[test]
    fn latest_labels_rolls_back_and_ignores_late_reversed_tips() {
        let mut latest = LatestLabels::default();
        let t1 = Event::live(
            Platform::Tips,
            Some("tip-1".into()),
            Millis(1_000),
            EventKind::Tip {
                actor: who("FirstTipper"),
                money: Money::new(1_000, "USD"),
                message: None,
                provider: TipProvider::Stripe,
                payment_ref: "ref-1".into(),
                status: PaymentStatus::Captured,
            },
        );
        let t2 = Event::live(
            Platform::Tips,
            Some("tip-2".into()),
            Millis(2_000),
            EventKind::Tip {
                actor: who("BadTipper"),
                money: Money::new(5_000, "USD"),
                message: None,
                provider: TipProvider::Stripe,
                payment_ref: "ref-2".into(),
                status: PaymentStatus::Captured,
            },
        );
        let rev2 = Event::live(
            Platform::Tips,
            Some("rev-2".into()),
            Millis(3_000),
            EventKind::Reversal { original_source_id: "tip-2".into(), reason: ReversalReason::Chargeback },
        );
        let late_rev = Event::live(
            Platform::Tips,
            Some("rev-3".into()),
            Millis(4_000),
            EventKind::Reversal { original_source_id: "tip-3".into(), reason: ReversalReason::Refund },
        );
        let t3_late = Event::live(
            Platform::Tips,
            Some("tip-3".into()),
            Millis(4_500),
            EventKind::Tip {
                actor: who("AlreadyReversed"),
                money: Money::new(9_000, "USD"),
                message: None,
                provider: TipProvider::Stripe,
                payment_ref: "ref-3".into(),
                status: PaymentStatus::Captured,
            },
        );

        latest.observe(&t1);
        latest.observe(&t2);
        assert_eq!(latest.label(LabelSource::LatestTip).as_deref(), Some("BadTipper $50"));
        latest.observe(&rev2);
        assert_eq!(latest.label(LabelSource::LatestTip).as_deref(), Some("FirstTipper $10"));

        // Reversal observed before delayed duplicate/replayed tip
        latest.observe(&late_rev);
        latest.observe(&t3_late);
        assert_eq!(latest.label(LabelSource::LatestTip).as_deref(), Some("FirstTipper $10"));
    }

    #[test]
    fn cross_currency_ranking_uses_coarse_usd_without_converting_display() {
        let mut p = PeriodTotals::default();
        p.observe(&tip(1_000, "jp1", "TokyoFan", 1_000, "JPY", PaymentStatus::Captured));
        // Displayed unconverted when alone
        assert_eq!(p.top_tip_display(), "TokyoFan ¥1,000");

        // $10 USD (1000 cents) beats ¥1,000 JPY (~$6.50) in both top tip and supporter rank
        p.observe(&tip(2_000, "us1", "AustinFan", 1_000, "USD", PaymentStatus::Captured));
        assert_eq!(p.top_tip_display(), "AustinFan $10");
        assert!(p.top_supporter_display().starts_with("AustinFan"));
    }

    #[test]
    fn anonymous_tips_do_not_aggregate_and_non_positive_tips_are_ignored() {
        let mut period = PeriodTotals::default();
        let mut latest = LatestLabels::default();
        let mut train = ComboTrain::new(ComboTrainConfig { window_s: 60, step_s: 15, max_s: 90, threshold: 2 });

        let mut anon1 = tip(1_000, "a1", "", 1_000, "USD", PaymentStatus::Captured);
        if let EventKind::Tip { ref mut actor, .. } = anon1.kind {
            *actor = Actor::anonymous(Platform::Tips);
        }
        let mut anon2 = tip(2_000, "a2", "", 1_000, "USD", PaymentStatus::Captured);
        if let EventKind::Tip { ref mut actor, .. } = anon2.kind {
            *actor = Actor::anonymous(Platform::Tips);
        }
        let named = tip(3_000, "n1", "NamedFan", 1_500, "USD", PaymentStatus::Captured);
        let zero_tip = tip(4_000, "z1", "ZeroFan", 0, "USD", PaymentStatus::Captured);
        let neg_tip = tip(5_000, "neg1", "NegFan", -500, "USD", PaymentStatus::Captured);

        for ev in [&anon1, &anon2] {
            period.observe(ev);
        }
        // Two anonymous $10 tips must NOT combine into "Anonymous $20"
        assert_eq!(period.top_tip_display(), "Anonymous $10");
        period.observe(&named);
        assert_eq!(period.top_tip_display(), "NamedFan $15");

        for ev in [&anon1, &zero_tip, &neg_tip] {
            period.observe(ev);
            latest.observe(ev);
            train.observe(ev);
        }
        assert_eq!(latest.label(LabelSource::LatestTip).as_deref(), Some("Anonymous $10"));
        assert!(!train.view(Millis(5_000)).active, "zero/neg tips must not ignite 2-event train");
        assert_eq!(train.view(Millis(5_000)).warmup_count, 1);
    }

    #[test]
    fn hype_train_never_shrinks_ends_at_and_deactivates_below_threshold_on_reversal() {
        let cfg = ComboTrainConfig { window_s: 60, step_s: 15, max_s: 90, threshold: 3 };
        let mut train = ComboTrain::new(cfg);

        // Ignite at t=100_000 -> ends_at = 160_000; push to 190_000
        train.observe(&follow(90_000, "A"));
        train.observe(&follow(95_000, "B"));
        train.observe(&follow(100_000, "C"));
        train.observe(&follow(100_000, "D"));
        train.observe(&follow(100_000, "E"));
        assert_eq!(train.view(Millis(100_000)).ends_at, Some(Millis(190_000)));

        // Out-of-order delayed event with older occurred_at=10_000 must NOT shrink ends_at!
        train.observe(&follow(10_000, "OutOfOrder"));
        assert_eq!(train.view(Millis(100_000)).ends_at, Some(Millis(190_000)));

        // Reversing a train-igniting tip so events drop below threshold must deactivate the train
        let mut tip_train = ComboTrain::new(cfg);
        tip_train.observe(&follow(10_000, "X"));
        tip_train.observe(&follow(15_000, "Y"));
        let rev_target = tip(20_000, "tip-ignite", "Z", 500, "USD", PaymentStatus::Captured);
        tip_train.observe(&rev_target);
        assert!(tip_train.view(Millis(20_000)).active);

        tip_train.reverse("tip-ignite", Millis(25_000));
        let v = tip_train.view(Millis(25_000));
        assert!(!v.active, "reversed tip leaving < threshold events must deactivate active train");
        assert_eq!(v.warmup_count, 2);
    }

    #[test]
    fn an_expired_pulse_behind_a_fresh_one_does_not_start_a_train() {
        let cfg = ComboTrainConfig { window_s: 60, step_s: 15, max_s: 90, threshold: 3 };
        let mut train = ComboTrain::new(cfg);
        // Arrivals out of order: 50 s, then 0 s, then 70 s. At 70 s the 0 s pulse is 70 s old.
        train.observe_at(&follow(50_000, "A"), Millis(50_000));
        train.observe_at(&follow(0, "B"), Millis(50_000).max(Millis(0)));
        train.observe_at(&follow(70_000, "C"), Millis(70_000));
        assert!(!train.view(Millis(70_000)).active, "only two pulses are inside the window");
    }
}
