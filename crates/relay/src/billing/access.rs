//! What Pro costs and when a subscription grants it. Pure functions, no I/O: the relay
//! copies Razorpay's view of a subscription and asks these what access it means.

/// RBI e-mandate rules: a renewal above ₹15,000 needs the customer's OTP each time, which
/// breaks auto-renew. Every single charge stays below it.
pub const MANDATE_NO_OTP_LIMIT_PAISE: i64 = 1_500_000; // ₹15,000

/// A renewal that is late (Razorpay retrying the card or UPI mandate) keeps Pro this long
/// past the paid period.
pub const GRACE_MS: i64 = 3 * 86_400_000;

/// The two ways to pay for Ferrocast Pro, in INR (R-68).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    Monthly,
    Yearly,
}

impl Plan {
    pub const ALL: [Plan; 2] = [Plan::Monthly, Plan::Yearly];

    pub fn parse(s: &str) -> Option<Plan> {
        match s {
            "monthly" => Some(Plan::Monthly),
            "yearly" => Some(Plan::Yearly),
            _ => None,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Plan::Monthly => "monthly",
            Plan::Yearly => "yearly",
        }
    }

    /// Razorpay's plan `period`.
    pub fn period(self) -> &'static str {
        self.key()
    }

    /// The amount charged each cycle, in paise (₹1 = 100 paise). The relay is the only
    /// source of amounts charged; the website's PRO prices must match these.
    pub fn amount_paise(self) -> i64 {
        match self {
            Plan::Monthly => 19_900, // ₹199
            Plan::Yearly => 199_900, // ₹1,999
        }
    }

    /// Billing cycles before Razorpay ends the subscription (10 years either way).
    pub fn total_count(self) -> i64 {
        match self {
            Plan::Monthly => 120,
            Plan::Yearly => 10,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Plan::Monthly => "Ferrocast Pro (monthly)",
            Plan::Yearly => "Ferrocast Pro (yearly)",
        }
    }
}

/// Razorpay subscription statuses, grouped by what they mean for access.
/// Paid and in force (`pending` = a renewal being retried).
pub const PAID: [&str; 2] = ["active", "pending"];
/// Ended, but the period already paid for is honoured.
pub const ENDED: [&str; 2] = ["cancelled", "completed"];
/// A subscription in one of these blocks starting another one.
pub const BLOCKING: [&str; 3] = ["authenticated", "active", "pending"];
/// Not finished: worth asking Razorpay about again.
pub const OPEN: [&str; 6] = ["created", "authenticated", "active", "pending", "halted", "paused"];

/// The relay's copy of one subscription, as far as access goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubState {
    pub status: String,
    /// End of the current paid period (ms).
    pub current_end: Option<i64>,
    /// The streamer asked to stop renewing; no grace after the period then.
    pub cancel_at_end: bool,
    /// A payment was refunded in full: access ends at once.
    pub refunded: bool,
}

/// Until when this subscription grants Pro (ms), or `None` for no access at all.
///
/// - `active`, `pending`: the paid period, plus [`GRACE_MS`] for a late renewal unless it
///   was cancelled to end with the period.
/// - `cancelled`, `completed`: the period already paid for.
/// - `created`, `authenticated` (not charged yet), `halted` (every retry failed),
///   `paused`, `expired`, anything unknown, or refunded: none.
pub fn access_until(s: &SubState) -> Option<i64> {
    if s.refunded {
        return None;
    }
    let end = s.current_end?;
    if PAID.contains(&s.status.as_str()) {
        return Some(if s.cancel_at_end { end } else { end.saturating_add(GRACE_MS) });
    }
    if ENDED.contains(&s.status.as_str()) {
        return Some(end);
    }
    None
}

/// Whether the subscription grants Pro at `now`.
pub fn is_pro(s: &SubState, now: i64) -> bool {
    access_until(s).is_some_and(|until| now < until)
}
