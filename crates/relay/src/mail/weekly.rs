//! The weekly job (hourly, from `main.rs`): each confirmed address whose subscription has Pro
//! gets the report on its day ([`super::send_day`]) once that day's [`super::SEND_HOUR`] has
//! passed, at most once per (subscription, week), up to [`super::CATCH_UP_MS`] late (last
//! week's moment still counts early in the next week). Then the sweep: addresses go 30 days
//! after Pro ended, unconfirmed ones a week after their link expired, old `mail_sent` rows
//! after 120 days.
//!
//! Once per week, even across restarts: (sub, week) is claimed in `mail_sent` *before* the
//! send, and the row is read again after the claim (an address changed or stopped meanwhile
//! is left alone). A send the provider didn't accept (no connection, 503, its daily limit,
//! our account refused) gives the claim back for a later run; one whose answer was lost
//! (a read timeout, other 5xx) or a crash between the send and marking it keeps the claim,
//! so it counts as sent (at most once, never twice).
//!
//! What stops a run: our account refused, the provider's limit or the relay's daily budget
//! reached (the rest go tomorrow, inside the catch-up window), the provider down, an answer
//! lost, two different addresses refused one after the other (more likely us than them: the
//! second isn't paused), or the vault not opening any address (nothing is paused for it).

use super::report;
use super::store::{Addr, CLAIMED, MailRow, SKIPPED};
use super::{Email, MailError, PURPOSE, SEND_TIMEOUT, WEEK_MS, budget_day, due_moments};
use crate::AppState;
use crate::billing::access;
use crate::history::{KEEP_AFTER_LAPSE_MS, week_start};

/// At most this many reports per run (an hour apart): a burst stays gentle on the provider.
pub const MAX_PER_RUN: usize = 500;
/// An unconfirmed address goes this long after its link expired.
const PENDING_KEEP_MS: i64 = 6 * 86_400_000;
/// `mail_sent` rows older than this aren't needed any more.
const SENT_KEEP_MS: i64 = 120 * 86_400_000;
/// When one address doesn't open, this many others are tried to tell "this row" from "the
/// vault".
const PROBE: usize = 20;

/// What one run did (for the log and tests).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RunStats {
    pub sent: usize,
    /// Weeks without a stream (nothing sent, on purpose; looked at again while late uploads
    /// can still count).
    pub skipped: usize,
    /// The provider refused the address, or it alone didn't open: paused.
    pub paused: usize,
    /// Given back for a later run (the provider down or refusing our account, its limit, the
    /// vault, a second refused address).
    pub retry: usize,
    /// The answer was lost: counted as sent, never sent again.
    pub unsure: usize,
    /// Left for tomorrow: the relay's daily budget was used up.
    pub deferred: usize,
}

fn pro_now(st: &AppState, sub: &str, now: i64) -> Option<bool> {
    match st.db.pro_sub(sub) {
        Ok(Some(s)) => Some(access::is_pro(&s.access(), now)),
        Ok(None) => Some(false),
        Err(e) => {
            tracing::warn!(error = %e, "couldn't read a subscription for the weekly email");
            None
        }
    }
}

fn open(st: &AppState, sub: &str, a: &Addr) -> Option<String> {
    let bytes = st.vault().ok()?.open(sub, PURPOSE, &a.sealed).ok()?;
    String::from_utf8(bytes).ok()
}

/// Whether some other address in this run opens (so the one that didn't is alone).
fn others_open(st: &AppState, rows: &[MailRow], sub: &str) -> bool {
    rows.iter()
        .filter(|r| r.sub_id != sub)
        .filter_map(|r| r.on().map(|a| (r, a)))
        .take(PROBE)
        .any(|(r, a)| open(st, &r.sub_id, a).is_some())
}

/// What happened to one row.
enum Step {
    Next,
    Stop,
}

struct Run<'a> {
    st: &'a AppState,
    now: i64,
    day: i64,
    stats: RunStats,
    /// An address opened this run (so one that doesn't is that row's problem).
    opened: bool,
    /// The previous send was refused, for this subscription.
    refused: Option<String>,
}

impl Run<'_> {
    fn unclaim(&self, sub: &str, week: i64) {
        if let Err(e) = self.st.db.mail_unclaim(sub, week) {
            tracing::warn!(error = %e, "couldn't give back a weekly email claim");
        }
    }

    fn give_budget_back(&self) {
        if let Err(e) = self.st.db.mail_budget_give_back(self.day) {
            tracing::warn!(error = %e, "couldn't give back the daily mail budget");
        }
    }

    async fn row(&mut self, row: &MailRow, rows: &[MailRow]) -> Step {
        let st = self.st;
        let now = self.now;
        let sub = row.sub_id.as_str();
        let Some(addr) = row.on() else { return Step::Next };
        for due in due_moments(sub, now) {
            // Confirmed after this moment: the first report is the next one.
            if row.confirmed_at.is_some_and(|c| c > due) {
                continue;
            }
            let week = week_start(due);
            let prior = match st.db.mail_sent_at(sub, week) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "couldn't read the weekly email log");
                    continue;
                }
            };
            // Sent, claimed or lost already; a skipped week is looked at again.
            if prior.is_some_and(|p| p != SKIPPED) {
                continue;
            }
            if pro_now(st, sub, now) != Some(true) {
                continue;
            }
            let (from, to) = (due - WEEK_MS, due);
            let (this, before) = match (
                st.db.list_streams(sub, from, to, None, 10_000),
                st.db.list_streams(sub, from - WEEK_MS, from, None, 10_000),
            ) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(e), _) | (_, Err(e)) => {
                    tracing::warn!(error = %e, "couldn't read history for the weekly email");
                    continue;
                }
            };
            let Some(rep) = report::build(&this, &before, from, to) else {
                // No streams: nothing is sent. Recorded once; looked at again until the
                // catch-up window ends, so a stream uploaded late still gets its report.
                if prior.is_none() && st.db.mail_claim(sub, week, SKIPPED).unwrap_or(false) {
                    self.stats.skipped += 1;
                }
                continue;
            };
            match st.db.mail_claim(sub, week, CLAIMED) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    tracing::warn!(error = %e, "couldn't claim a weekly email");
                    continue;
                }
            }
            // The address may have changed or stopped since the list was read.
            match st.db.mail_row(sub) {
                Ok(Some(fresh)) if fresh.on() == Some(addr) => {}
                Ok(_) => {
                    self.unclaim(sub, week);
                    return Step::Next;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "couldn't read a weekly email address again");
                    self.unclaim(sub, week);
                    return Step::Next;
                }
            }
            let Some(address) = open(st, sub, addr) else {
                if self.opened || others_open(st, rows, sub) {
                    tracing::error!(
                        "one weekly email address didn't open while others did; it's paused until saved again"
                    );
                    let _ = st.db.mail_pause(sub, &addr.tag, "unreadable", now);
                    self.stats.paused += 1;
                    return Step::Next;
                }
                self.unclaim(sub, week);
                tracing::error!(
                    "weekly email addresses don't open (check RELAY_VAULT_KEY); nothing is paused and the run stops"
                );
                self.stats.retry += 1;
                return Step::Stop;
            };
            self.opened = true;
            match st.db.mail_budget_take(self.day, st.mail.cfg.budget) {
                Ok(true) => {}
                Ok(false) => {
                    self.unclaim(sub, week);
                    tracing::info!(
                        "the relay's daily email budget is used up; the rest of the weekly emails go tomorrow"
                    );
                    self.stats.deferred += 1;
                    return Step::Stop;
                }
                Err(e) => {
                    self.unclaim(sub, week);
                    tracing::warn!(error = %e, "couldn't count the daily mail budget");
                    self.stats.retry += 1;
                    return Step::Stop;
                }
            }
            let Some(token) = st.mail.cfg.unsubscribe_token(sub, &addr.tag) else {
                self.unclaim(sub, week);
                self.give_budget_back();
                return Step::Stop;
            };
            let unsubscribe = format!("{}/mail/unsubscribe?t={token}", st.cfg.public_url);
            // The web dashboard's History, when this relay serves one.
            let history = st
                .remote
                .cfg
                .origin
                .as_deref()
                .filter(|_| st.remote.cfg.dashboard_dir.is_some())
                .map(|o| format!("{o}/app/#/history"));
            let r = report::render_with(&rep, &unsubscribe, history.as_deref());
            let email = Email {
                to: address,
                subject: r.subject,
                html: r.html,
                text: r.text,
                headers: vec![
                    ("List-Unsubscribe".into(), format!("<{unsubscribe}>")),
                    ("List-Unsubscribe-Post".into(), "List-Unsubscribe=One-Click".into()),
                ],
                tag: "weekly",
            };
            let Some(mailer) = st.mail.mailer.clone() else { return Step::Stop };
            let result = match tokio::time::timeout(SEND_TIMEOUT, mailer.send(&st.mail.cfg.from, &email)).await {
                Ok(r) => r,
                Err(_) => Err(MailError::Uncertain("timed out".into())),
            };
            return match result {
                Ok(()) => {
                    if let Err(e) = st.db.mail_mark_sent(sub, week, now) {
                        // Still claimed, so it isn't sent again.
                        tracing::warn!(error = %e, "a weekly email went but couldn't be marked sent");
                    }
                    self.stats.sent += 1;
                    self.refused = None;
                    Step::Next
                }
                Err(MailError::Rejected(why)) => {
                    self.give_budget_back();
                    if self.refused.as_deref().is_some_and(|s| s != sub) {
                        // Two different addresses in a row: more likely our request than
                        // their addresses. The second isn't paused; the run stops.
                        self.unclaim(sub, week);
                        tracing::error!(why = %why, "the mail provider refused two addresses in a row; the run stops and the second isn't paused");
                        self.stats.retry += 1;
                        return Step::Stop;
                    }
                    tracing::warn!(why = %why, "the mail provider refused a weekly email's recipient; that address is paused");
                    let _ = st.db.mail_pause(sub, &addr.tag, "rejected", now);
                    self.stats.paused += 1;
                    self.refused = Some(sub.to_string());
                    Step::Next
                }
                Err(MailError::Misconfigured(why)) => {
                    // Our account or request, not this address: give the week back, stop.
                    self.unclaim(sub, week);
                    self.give_budget_back();
                    tracing::error!(why = %why, "the mail provider refused the relay's account: check BREVO_API_KEY and the sender");
                    self.stats.retry += 1;
                    Step::Stop
                }
                Err(MailError::Limited(why)) => {
                    // Nothing more today, confirms included; the rest go tomorrow.
                    self.unclaim(sub, week);
                    if let Err(e) = st.db.mail_budget_exhaust(self.day, st.mail.cfg.budget) {
                        tracing::warn!(error = %e, "couldn't stop today's mail budget");
                    }
                    tracing::error!(why = %why, "the mail provider's sending limit is reached; weekly emails continue tomorrow");
                    self.stats.retry += 1;
                    Step::Stop
                }
                Err(MailError::Unavailable(why)) => {
                    self.unclaim(sub, week);
                    self.give_budget_back();
                    tracing::warn!(why = %why, "the mail provider is unavailable; weekly emails wait for the next run");
                    self.stats.retry += 1;
                    Step::Stop
                }
                Err(MailError::Uncertain(why)) => {
                    // It may have gone: the claim stays (never twice), the budget counts it.
                    tracing::warn!(why = %why, "a weekly email's answer was lost; it counts as sent");
                    self.stats.unsure += 1;
                    Step::Stop
                }
            };
        }
        Step::Next
    }
}

/// Sends every report that's due at `now`.
pub async fn run_weekly(st: &AppState, now: i64) -> RunStats {
    if st.mail.mailer.is_none() || !super::available(st) {
        return RunStats::default();
    }
    let rows = match st.db.mail_on_rows() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list weekly email addresses");
            return RunStats::default();
        }
    };
    let mut run = Run { st, now, day: budget_day(now), stats: RunStats::default(), opened: false, refused: None };
    for row in &rows {
        if run.stats.sent >= MAX_PER_RUN {
            break;
        }
        if let Step::Stop = run.row(row, &rows).await {
            break;
        }
    }
    let stats = run.stats;
    if stats != RunStats::default() {
        tracing::info!(
            sent = stats.sent,
            skipped = stats.skipped,
            paused = stats.paused,
            retry = stats.retry,
            unsure = stats.unsure,
            deferred = stats.deferred,
            "weekly emails"
        );
    }
    stats
}

/// The hourly housekeeping: lapsed subscriptions' addresses after 30 days, unconfirmed ones
/// a week after their link expired, old send records.
pub async fn sweep(st: &AppState, now: i64) {
    match st.db.mail_subs() {
        Ok(subs) => {
            for sub in subs {
                let keep = match st.db.pro_sub(&sub) {
                    Ok(None) => false,
                    Ok(Some(s)) => {
                        let a = s.access();
                        access::is_pro(&a, now) || {
                            let stopped = s.stopped_at();
                            now < stopped.saturating_add(KEEP_AFTER_LAPSE_MS)
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "couldn't check a weekly email's subscription");
                        continue;
                    }
                };
                if !keep {
                    match st.db.mail_forget(&sub) {
                        Ok(()) => tracing::info!("a weekly email address was deleted 30 days after Pro ended"),
                        Err(e) => tracing::warn!(error = %e, "couldn't delete a lapsed weekly email address"),
                    }
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "couldn't list weekly email addresses"),
    }
    if let Err(e) = st.db.mail_drop_unconfirmed(now - PENDING_KEEP_MS) {
        tracing::warn!(error = %e, "couldn't drop unconfirmed weekly email addresses");
    }
    if let Err(e) = st.db.mail_prune_sent(now - SENT_KEEP_MS) {
        tracing::warn!(error = %e, "couldn't prune the weekly email log");
    }
    if let Err(e) = st.db.mail_budget_prune(budget_day(now) - 7) {
        tracing::warn!(error = %e, "couldn't prune the daily mail budget");
    }
}
