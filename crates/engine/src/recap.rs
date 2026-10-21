//! The end-of-stream recap, from the live session (its totals already count refunds and
//! the viewer peak) and the event log (the lists).

use ferrocast_core::event::Millis;
use ferrocast_core::recap::{self, Recap, Totals};
use ferrocast_store::repo;

use crate::{Engine, EngineError};

impl Engine {
    /// The recap since `since`, or for this (or the last) stream. With no stream on record
    /// it's empty, with `started_at: null`.
    pub async fn recap(&self, since: Option<i64>) -> Result<Recap, EngineError> {
        let (session, live) = {
            let st = self.state();
            (st.session.clone(), st.live.values().any(|l| *l))
        };
        let start = match since.or(session.started_at.map(|m| m.0)) {
            Some(s) => Some(s),
            None => self.db(repo::recap::last_online).await?,
        };
        let Some(start) = start else {
            return Ok(recap::build(None, None, live, &[], vec![], 0, None));
        };
        let (events, (messages, top), ended) = self
            .db(move |s| {
                Ok((
                    repo::recap::support_since(s, start)?,
                    repo::recap::chat_since(s, start, 10)?,
                    repo::recap::offline_after(s, start)?,
                ))
            })
            .await?;
        let this_session = session.started_at.is_some_and(|m| m.0 == start);
        let totals = this_session.then(|| Totals {
            follows: u64::from(session.follows),
            subs: u64::from(session.subs),
            tips: session.tips.clone(),
            bits: session.bits,
            messages: session.chat_lines.values().map(|n| u64::from(*n)).sum(),
            peak_viewers: session.peak_viewers,
        });
        let ended_at = if live { None } else { ended.map(Millis) };
        Ok(recap::build(Some(Millis(start)), ended_at, live, &events, top, messages, totals))
    }
}

impl Engine {
    /// Top supporters in `[from, to)` for Insights (`GET /api/v1/insights/supporters`), from
    /// this PC's own event log: subscribers and gifters, tippers (each currency on its own,
    /// never converted), raiders and the most active chatters. Local data,
    /// so it works without an account; the dashboard asks for the same range as the history.
    pub async fn supporters(&self, from: i64, to: Option<i64>) -> Result<serde_json::Value, EngineError> {
        let to = to.unwrap_or_else(|| crate::now().0);
        if to <= from {
            return Err(EngineError::Invalid("from must be before to.".into()));
        }
        let (events, (messages, top)) = self
            .db(move |s| Ok((repo::recap::support_since(s, from)?, repo::recap::chat_between(s, from, to, 10)?)))
            .await?;
        let truncated = events.len() >= 5_000;
        let events: Vec<_> = events.into_iter().filter(|e| e.occurred_at.0 < to).collect();
        let r = recap::build(Some(Millis(from)), Some(Millis(to)), false, &events, top, messages, None);
        let mut subs = r.subscribers;
        subs.sort_by(|a, b| b.gifts.cmp(&a.gifts).then(b.months.cmp(&a.months)));
        subs.truncate(20);
        let mut tippers = r.tippers;
        let biggest = |t: &recap::Tipper| t.amounts.iter().map(|m| m.minor).max().unwrap_or(0);
        tippers.sort_by(|a, b| b.bits.cmp(&a.bits).then(biggest(b).cmp(&biggest(a))));
        tippers.truncate(20);
        let mut raiders = r.raiders;
        raiders.sort_by_key(|r| std::cmp::Reverse(r.viewers));
        raiders.truncate(20);
        Ok(serde_json::json!({
            "from": from, "to": to, "truncated": truncated,
            "totals": r.totals, "new_followers": r.new_followers.len(),
            "subscribers": subs, "tippers": tippers, "raiders": raiders, "top_chatters": r.top_chatters,
        }))
    }
}
