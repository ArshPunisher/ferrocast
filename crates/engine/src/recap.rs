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
