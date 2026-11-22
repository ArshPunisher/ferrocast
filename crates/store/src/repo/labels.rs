//! SQLite reads for local stream labels (session, monthly, all-time, and latest-X).

use std::collections::HashSet;

use ferrocast_core::event::{Event, EventKind, Millis, Origin, PaymentStatus};
use ferrocast_core::labels::{LatestLabels, PeriodTotals, summarize_events};
use rusqlite::params;

use crate::Store;
use crate::error::Result;

const SUPPORT_KINDS: &[&str] =
    &["follow", "subscription", "gift_batch", "tip", "paid_message", "platform_gift", "raid"];

fn row_to_event(r: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    let platform: String = r.get(1)?;
    let origin: String = r.get(3)?;
    let seq: i64 = r.get(7)?;
    Ok(Event {
        id: r.get(0)?,
        seq: u64::try_from(seq).unwrap_or(0),
        platform: super::from_json(1, format!("\"{platform}\""))?,
        channel: None,
        source_id: r.get(2)?,
        occurred_at: Millis(r.get(4)?),
        received_at: Millis(r.get(5)?),
        origin: super::from_json(3, format!("\"{origin}\""))?,
        kind: super::from_json(6, r.get(6)?)?,
        raw: None,
    })
}

/// Computes local label totals across `[from_ms, to_ms)` (or all-time when `None`).
///
/// Always includes live reversals even if a refund landed after `to_ms`, so refunded tips inside
/// the queried window are never counted.
pub fn period_totals(store: &Store, from_ms: Option<i64>, to_ms: Option<i64>) -> Result<PeriodTotals> {
    let kinds = serde_json::to_string(SUPPORT_KINDS)?;
    store.read(|c| {
        let mut st = c.prepare(
            "SELECT id, platform, source_id, origin, occurred_at, received_at, data, seq
             FROM events
             WHERE origin = 'live' AND hidden = 0
               AND (
                 (kind IN (SELECT value FROM json_each(?1))
                   AND (?2 IS NULL OR occurred_at >= ?2)
                   AND (?3 IS NULL OR occurred_at < ?3))
                 OR kind = 'reversal'
               )
             ORDER BY seq ASC",
        )?;
        let rows = st.query_map(params![kinds, from_ms, to_ms], row_to_event)?;
        let events = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(summarize_events(&events))
    })
}

/// Hydrates latest-X labels (follower, sub, gifter, tip, bits, raid) from the local SQLite log.
pub fn latest_labels(store: &Store) -> Result<LatestLabels> {
    store.read(|c| {
        let reversed: HashSet<String> = {
            let mut st = c.prepare(
                "SELECT json_extract(data, '$.original_source_id') FROM events
                 WHERE origin = 'live' AND hidden = 0 AND kind = 'reversal'",
            )?;
            let rows = st.query_map([], |r| r.get::<_, Option<String>>(0))?;
            rows.filter_map(|r| r.ok().flatten()).collect()
        };

        let mut latest = LatestLabels::default();

        // Simple single-row kinds
        for kind in ["follow", "subscription", "gift_batch", "raid"] {
            let sql = if kind == "subscription" {
                "SELECT id, platform, source_id, origin, occurred_at, received_at, data, seq
                 FROM events
                 WHERE origin = 'live' AND hidden = 0 AND kind = ?1 AND json_extract(data, '$.gifted_by') IS NULL
                 ORDER BY seq DESC LIMIT 1"
            } else {
                "SELECT id, platform, source_id, origin, occurred_at, received_at, data, seq
                 FROM events
                 WHERE origin = 'live' AND hidden = 0 AND kind = ?1
                 ORDER BY seq DESC LIMIT 1"
            };
            let mut st = c.prepare(sql)?;
            let mut rows = st.query_map([kind], row_to_event)?;
            if let Some(ev) = rows.next().transpose()? {
                latest.observe(&ev);
            }
        }

        // Latest captured non-reversed tip or paid_message
        {
            let mut st = c.prepare(
                "SELECT id, platform, source_id, origin, occurred_at, received_at, data, seq
                 FROM events
                 WHERE origin = 'live' AND hidden = 0 AND kind IN ('tip', 'paid_message')
                 ORDER BY seq DESC LIMIT 50",
            )?;
            let rows = st.query_map([], row_to_event)?;
            for ev in rows {
                let ev = ev?;
                if ev.origin != Origin::Live {
                    continue;
                }
                if ev.source_id.as_deref().is_some_and(|s| reversed.contains(s)) {
                    continue;
                }
                match &ev.kind {
                    EventKind::Tip { status: PaymentStatus::Captured, .. } | EventKind::PaidMessage { .. } => {
                        latest.observe(&ev);
                        break;
                    }
                    _ => {}
                }
            }
        }

        // Latest bits event (checking recent platform_gift rows)
        {
            let mut st = c.prepare(
                "SELECT id, platform, source_id, origin, occurred_at, received_at, data, seq
                 FROM events
                 WHERE origin = 'live' AND hidden = 0 AND kind = 'platform_gift'
                 ORDER BY seq DESC LIMIT 50",
            )?;
            let rows = st.query_map([], row_to_event)?;
            for ev in rows {
                let ev = ev?;
                if let EventKind::PlatformGift { value, .. } = &ev.kind
                    && value.unit == ferrocast_core::money::ValueUnit::Bits
                {
                    latest.observe(&ev);
                    break;
                }
            }
        }

        Ok(latest)
    })
}

#[cfg(test)]
mod tests {
    use ferrocast_core::event::{Actor, ReversalReason, Tier, TipProvider};
    use ferrocast_core::labels::month_bounds;
    use ferrocast_core::money::{Money, PlatformValue, ValueUnit};
    use ferrocast_core::overlay::LabelSource;

    use crate::repo::events::insert;

    use super::*;

    fn ev(id: &str, at: i64, kind: EventKind) -> Event {
        let mut e = Event::live(ferrocast_core::event::Platform::Twitch, Some(id.into()), Millis(at), kind);
        e.id = id.into();
        e
    }

    #[test]
    fn queries_monthly_and_alltime_across_month_boundary_and_hydrates_latest() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();

        let jan_end = 1_769_903_999_999_i64; // 2026-01-31T23:59:59.999Z
        let feb_1 = 1_769_904_000_000_i64; // 2026-02-01T00:00:00.000Z
        let (m_start, m_end) = month_bounds(Millis(feb_1), 0);
        assert_eq!(m_start.0, feb_1);

        let a = |n: &str| {
            let mut actor = Actor::named(ferrocast_core::event::Platform::Twitch, n);
            actor.id = Some(n.into());
            actor
        };

        insert(&store, &ev("f-jan", jan_end, EventKind::Follow { actor: a("JanFan") })).unwrap();
        insert(
            &store,
            &ev(
                "tip-jan",
                jan_end - 1000,
                EventKind::Tip {
                    actor: a("OldWhale"),
                    money: Money::new(5000, "USD"),
                    message: None,
                    provider: TipProvider::Stripe,
                    payment_ref: "tip-jan".into(),
                    status: PaymentStatus::Captured,
                },
            ),
        )
        .unwrap();
        insert(&store, &ev("f-feb", feb_1, EventKind::Follow { actor: a("FebFan") })).unwrap();
        insert(
            &store,
            &ev(
                "s-feb",
                feb_1 + 1000,
                EventKind::Subscription {
                    actor: a("FebSub"),
                    tier: Tier::Tier1,
                    months: 1,
                    gifted_by: None,
                    message: None,
                },
            ),
        )
        .unwrap();
        insert(
            &store,
            &ev(
                "b-feb",
                feb_1 + 2000,
                EventKind::PlatformGift {
                    actor: a("FebBits"),
                    value: PlatformValue { unit: ValueUnit::Bits, amount: 750 },
                    message: None,
                },
            ),
        )
        .unwrap();
        insert(&store, &ev("r-feb", feb_1 + 3000, EventKind::Raid { from: a("FebRaider"), viewers: 33 })).unwrap();
        // Refund the Jan tip later in February
        insert(
            &store,
            &ev(
                "rev-jan",
                feb_1 + 4000,
                EventKind::Reversal { original_source_id: "tip-jan".into(), reason: ReversalReason::Refund },
            ),
        )
        .unwrap();
        // Test event ignored everywhere
        let mut t = ev("test-f", feb_1 + 5000, EventKind::Follow { actor: a("TestIgnore") });
        t.origin = Origin::Test;
        insert(&store, &t).unwrap();

        let monthly = period_totals(&store, Some(m_start.0), Some(m_end.0)).unwrap();
        assert_eq!(monthly.follows, 1);
        assert_eq!(monthly.subs, 1);
        assert_eq!(monthly.bits, 750);
        assert_eq!(monthly.raids, 1);

        let alltime = period_totals(&store, None, None).unwrap();
        assert_eq!(alltime.follows, 2);
        assert_eq!(alltime.formatted_tips(), "0", "January tip was refunded in February");

        let latest = latest_labels(&store).unwrap();
        assert_eq!(latest.label(LabelSource::LatestFollower).as_deref(), Some("FebFan"));
        assert_eq!(latest.label(LabelSource::LatestSub).as_deref(), Some("FebSub"));
        assert_eq!(latest.label(LabelSource::LatestBits).as_deref(), Some("FebBits 750 bits"));
        assert_eq!(latest.label(LabelSource::LatestRaid).as_deref(), Some("FebRaider (33)"));
        assert_eq!(latest.label(LabelSource::LatestTip).as_deref(), Some(""));
    }

    #[test]
    fn latest_labels_excludes_gifted_subscription_recipients() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let base = 1_700_000_000_000i64;
        let a = |n: &str| {
            let mut actor = Actor::named(ferrocast_core::event::Platform::Twitch, n);
            actor.id = Some(n.into());
            actor
        };
        insert(
            &store,
            &ev(
                "s-own",
                base,
                EventKind::Subscription {
                    actor: a("OwnSub"),
                    tier: Tier::Tier1,
                    months: 1,
                    gifted_by: None,
                    message: None,
                },
            ),
        )
        .unwrap();
        insert(
            &store,
            &ev(
                "s-gifted",
                base + 1000,
                EventKind::Subscription {
                    actor: a("GiftedRecipient"),
                    tier: Tier::Tier1,
                    months: 1,
                    gifted_by: Some(a("Gifter")),
                    message: None,
                },
            ),
        )
        .unwrap();

        let latest = latest_labels(&store).unwrap();
        assert_eq!(latest.label(LabelSource::LatestSub).as_deref(), Some("OwnSub"));
    }
}
