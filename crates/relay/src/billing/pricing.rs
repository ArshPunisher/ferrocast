//! What Pro costs right now: the owner's prices (the defaults in [`Plan::amount_paise`] until
//! they're changed from the Control Center), timed sales, and the Razorpay plan behind each
//! amount. Razorpay ties a subscription to a plan for its whole life, so a new price, a sale
//! or a discount code gets a plan of its own (made through Razorpay's Plans API, which is
//! free) and only new checkouts use it: everyone who already pays keeps their plan and price.

use std::collections::BTreeMap;

use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use super::access::{self, Plan};
use crate::db::Db;

/// Migration 14: prices, sales, the plans made for them, codes, grant history, refunds and
/// the admin audit trail. Only adds tables, so an older relay still runs on it.
pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS pro_prices (id TEXT PRIMARY KEY, period TEXT NOT NULL, amount INTEGER NOT NULL, created_by TEXT NOT NULL, created_at INTEGER NOT NULL, retired_at INTEGER);
CREATE INDEX IF NOT EXISTS pro_prices_period ON pro_prices(period, created_at);
CREATE TABLE IF NOT EXISTS pro_rzp_plans (period TEXT NOT NULL, amount INTEGER NOT NULL, plan_id TEXT NOT NULL, created_at INTEGER NOT NULL, PRIMARY KEY (period, amount)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS pro_sales (id TEXT PRIMARY KEY, name TEXT NOT NULL, headline TEXT NOT NULL, message TEXT, discounts TEXT NOT NULL, prices TEXT NOT NULL, starts_at INTEGER NOT NULL, ends_at INTEGER, stopped_at INTEGER, show_countdown INTEGER NOT NULL, show_banner INTEGER NOT NULL, theme TEXT NOT NULL, created_by TEXT NOT NULL, updated_by TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS pro_codes (code TEXT PRIMARY KEY, kind TEXT NOT NULL, days INTEGER, percent_off INTEGER, applies_period TEXT, max_uses INTEGER, valid_until INTEGER, active INTEGER NOT NULL DEFAULT 1, note TEXT, created_by TEXT NOT NULL, created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS pro_code_uses (code TEXT NOT NULL, install_id TEXT NOT NULL, sub_id TEXT, at INTEGER NOT NULL, PRIMARY KEY (code, install_id)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS pro_sub_prices (sub_id TEXT PRIMARY KEY, period TEXT NOT NULL, amount INTEGER NOT NULL, list_amount INTEGER NOT NULL, plan_id TEXT NOT NULL, code TEXT, sale_id TEXT, at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS pro_grants (id INTEGER PRIMARY KEY AUTOINCREMENT, sub_id TEXT NOT NULL, install_id TEXT NOT NULL, kind TEXT NOT NULL, source TEXT NOT NULL, code TEXT, actor TEXT, note TEXT, days INTEGER NOT NULL, until INTEGER NOT NULL, at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS pro_grants_sub ON pro_grants(sub_id, at);
CREATE TABLE IF NOT EXISTS pro_refunds (refund_id TEXT PRIMARY KEY, payment_id TEXT NOT NULL, amount INTEGER NOT NULL, status TEXT NOT NULL, at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS pro_refunds_payment ON pro_refunds(payment_id);
CREATE TABLE IF NOT EXISTS admin_audit (id INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL, actor TEXT NOT NULL, action TEXT NOT NULL, target TEXT, detail TEXT NOT NULL);
";

/// The lowest price Razorpay takes (₹1).
pub const MIN_AMOUNT: i64 = 100;
/// Sale and code discounts, in percent.
pub const MAX_PERCENT: i64 = 90;
/// The looks a sale banner can have (the Control Center's sale editor offers these).
pub const THEMES: [&str; 5] = ["violet", "sunset", "emerald", "midnight", "festive"];

/// `pro_monthly` / `pro_yearly`: how sales and codes name a plan (the Control Center's keys).
pub fn key(plan: Plan) -> String {
    format!("pro_{}", plan.key())
}

/// `percent` off `list`, rounded down to whole rupees, never below ₹1.
pub fn discounted(list: i64, percent: i64) -> i64 {
    let pct = percent.clamp(0, 100);
    (list * (100 - pct) / 100 / 100 * 100).max(MIN_AMOUNT)
}

/// Is this a price Razorpay will charge without an OTP every renewal?
pub fn amount_ok(amount: i64) -> bool {
    (MIN_AMOUNT..access::MANDATE_NO_OTP_LIMIT_PAISE).contains(&amount)
}

/// One price change (the history the Control Center shows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceRow {
    pub id: String,
    pub period: String,
    pub amount: i64,
    pub created_by: String,
    pub created_at: i64,
    pub retired_at: Option<i64>,
}

/// One sale: a percent off (or a set price) per plan, between two times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaleRow {
    pub id: String,
    pub name: String,
    pub headline: String,
    pub message: Option<String>,
    /// Percent off per [`key`].
    pub discounts: BTreeMap<String, i64>,
    /// A set price (paise) per [`key`], instead of a percent.
    pub prices: BTreeMap<String, i64>,
    pub starts_at: i64,
    pub ends_at: Option<i64>,
    pub stopped_at: Option<i64>,
    pub show_countdown: bool,
    pub show_banner: bool,
    pub theme: String,
    pub created_by: String,
    pub updated_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl SaleRow {
    /// `scheduled`, `live`, `ended` or `stopped`.
    pub fn state(&self, now: i64) -> &'static str {
        if self.stopped_at.is_some() {
            "stopped"
        } else if self.ends_at.is_some_and(|e| e <= now) {
            "ended"
        } else if self.starts_at > now {
            "scheduled"
        } else {
            "live"
        }
    }

    /// The sale's price for `plan` when the list price is `list`; `None` if it isn't on sale
    /// or the sale wouldn't make it cheaper.
    pub fn amount(&self, plan: Plan, list: i64) -> Option<i64> {
        let k = key(plan);
        let a = match (self.prices.get(&k), self.discounts.get(&k)) {
            (Some(p), _) => *p,
            (None, Some(pct)) => discounted(list, *pct),
            (None, None) => return None,
        };
        (a < list).then_some(a)
    }

    /// The largest percent off ("up to"), set prices counted against `lists`.
    pub fn percent_off(&self, lists: &BTreeMap<String, i64>) -> i64 {
        let mut best = self.discounts.values().copied().max().unwrap_or(0);
        for (k, p) in &self.prices {
            if let Some(l) = lists.get(k).filter(|l| **l > 0) {
                best = best.max((l - p) * 100 / l);
            }
        }
        best
    }
}

/// What one plan costs a buyer right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quote {
    pub plan: Plan,
    /// The price without a sale or code.
    pub list: i64,
    /// What they pay.
    pub amount: i64,
    /// The sale that made it cheaper, if one did.
    pub sale: Option<String>,
    /// The discount code that made it cheaper, if one did.
    pub code: Option<String>,
}

fn sale_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SaleRow> {
    let map = |s: String| serde_json::from_str::<BTreeMap<String, i64>>(&s).unwrap_or_default();
    Ok(SaleRow {
        id: r.get(0)?,
        name: r.get(1)?,
        headline: r.get(2)?,
        message: r.get(3)?,
        discounts: map(r.get(4)?),
        prices: map(r.get(5)?),
        starts_at: r.get(6)?,
        ends_at: r.get(7)?,
        stopped_at: r.get(8)?,
        show_countdown: r.get::<_, i64>(9)? != 0,
        show_banner: r.get::<_, i64>(10)? != 0,
        theme: r.get(11)?,
        created_by: r.get(12)?,
        updated_by: r.get(13)?,
        created_at: r.get(14)?,
        updated_at: r.get(15)?,
    })
}

const SALE_COLS: &str = "id, name, headline, message, discounts, prices, starts_at, ends_at, stopped_at, show_countdown, \
                         show_banner, theme, created_by, updated_by, created_at, updated_at";

fn price_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PriceRow> {
    Ok(PriceRow {
        id: r.get(0)?,
        period: r.get(1)?,
        amount: r.get(2)?,
        created_by: r.get(3)?,
        created_at: r.get(4)?,
        retired_at: r.get(5)?,
    })
}

impl Db {
    /// The price in force for `plan` (paise) and the change that set it (`None`: the default).
    pub fn list_price(&self, plan: Plan) -> rusqlite::Result<(i64, Option<PriceRow>)> {
        let row = self
            .c()
            .query_row(
                "SELECT id, period, amount, created_by, created_at, retired_at FROM pro_prices
                   WHERE period = ?1 AND retired_at IS NULL ORDER BY created_at DESC, rowid DESC LIMIT 1",
                [plan.key()],
                price_row,
            )
            .optional()?;
        Ok((row.as_ref().map_or(plan.amount_paise(), |r| r.amount), row))
    }

    /// Both list prices, by [`key`].
    pub fn list_prices(&self) -> rusqlite::Result<BTreeMap<String, i64>> {
        let mut m = BTreeMap::new();
        for p in Plan::ALL {
            m.insert(key(p), self.list_price(p)?.0);
        }
        Ok(m)
    }

    /// Every price change, newest first.
    pub fn price_history(&self) -> rusqlite::Result<Vec<PriceRow>> {
        let c = self.c();
        let mut q = c.prepare(
            "SELECT id, period, amount, created_by, created_at, retired_at FROM pro_prices
               ORDER BY created_at DESC, rowid DESC LIMIT 200",
        )?;
        let rows = q.query_map([], price_row)?;
        rows.collect()
    }

    /// A new list price for `plan` (the one before is retired).
    pub fn set_price(&self, plan: Plan, amount: i64, actor: &str, now: i64) -> rusqlite::Result<PriceRow> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE pro_prices SET retired_at = ?2 WHERE period = ?1 AND retired_at IS NULL",
            params![plan.key(), now],
        )?;
        let id = format!("price_{}", crate::random(9));
        tx.execute(
            "INSERT INTO pro_prices(id, period, amount, created_by, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, plan.key(), amount, actor, now],
        )?;
        tx.commit()?;
        Ok(PriceRow {
            id,
            period: plan.key().into(),
            amount,
            created_by: actor.into(),
            created_at: now,
            retired_at: None,
        })
    }

    /// The Razorpay plan made for this amount, if there is one.
    pub fn rzp_plan(&self, plan: Plan, amount: i64) -> rusqlite::Result<Option<String>> {
        self.c()
            .query_row(
                "SELECT plan_id FROM pro_rzp_plans WHERE period = ?1 AND amount = ?2",
                params![plan.key(), amount],
                |r| r.get(0),
            )
            .optional()
    }

    pub fn put_rzp_plan(&self, plan: Plan, amount: i64, plan_id: &str, now: i64) -> rusqlite::Result<()> {
        self.c().execute(
            "INSERT OR IGNORE INTO pro_rzp_plans(period, amount, plan_id, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![plan.key(), amount, plan_id, now],
        )?;
        Ok(())
    }

    /// Every sale, newest first.
    pub fn sales(&self) -> rusqlite::Result<Vec<SaleRow>> {
        let c = self.c();
        let mut q =
            c.prepare(&format!("SELECT {SALE_COLS} FROM pro_sales ORDER BY starts_at DESC, created_at DESC"))?;
        let rows = q.query_map([], sale_row)?;
        rows.collect()
    }

    pub fn sale(&self, id: &str) -> rusqlite::Result<Option<SaleRow>> {
        self.c().query_row(&format!("SELECT {SALE_COLS} FROM pro_sales WHERE id = ?1"), [id], sale_row).optional()
    }

    /// The sale on at `now` (the one that started last, if two overlap).
    pub fn live_sale(&self, now: i64) -> rusqlite::Result<Option<SaleRow>> {
        self.c()
            .query_row(
                &format!(
                    "SELECT {SALE_COLS} FROM pro_sales WHERE stopped_at IS NULL AND starts_at <= ?1
                       AND (ends_at IS NULL OR ends_at > ?1) ORDER BY starts_at DESC LIMIT 1"
                ),
                [now],
                sale_row,
            )
            .optional()
    }

    /// Adds or replaces a sale.
    pub fn put_sale(&self, s: &SaleRow) -> rusqlite::Result<()> {
        let j = |m: &BTreeMap<String, i64>| serde_json::to_string(m).unwrap_or_else(|_| "{}".into());
        self.c().execute(
            &format!(
                "INSERT INTO pro_sales({SALE_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
                 ON CONFLICT(id) DO UPDATE SET name = excluded.name, headline = excluded.headline,
                   message = excluded.message, discounts = excluded.discounts, prices = excluded.prices,
                   starts_at = excluded.starts_at, ends_at = excluded.ends_at, stopped_at = excluded.stopped_at,
                   show_countdown = excluded.show_countdown, show_banner = excluded.show_banner,
                   theme = excluded.theme, updated_by = excluded.updated_by, updated_at = excluded.updated_at"
            ),
            params![
                s.id,
                s.name,
                s.headline,
                s.message,
                j(&s.discounts),
                j(&s.prices),
                s.starts_at,
                s.ends_at,
                s.stopped_at,
                i64::from(s.show_countdown),
                i64::from(s.show_banner),
                s.theme,
                s.created_by,
                s.updated_by,
                s.created_at,
                s.updated_at
            ],
        )?;
        Ok(())
    }

    /// What `plan` costs now, with the live sale and (if given and better) a discount code's
    /// percent off the list price.
    pub fn quote(&self, plan: Plan, now: i64, code: Option<(&str, i64)>) -> rusqlite::Result<Quote> {
        let (list, _) = self.list_price(plan)?;
        let mut q = Quote { plan, list, amount: list, sale: None, code: None };
        if let Some(s) = self.live_sale(now)?
            && let Some(a) = s.amount(plan, list)
        {
            q.amount = a;
            q.sale = Some(s.id);
        }
        if let Some((c, pct)) = code {
            let a = discounted(list, pct);
            if a < q.amount {
                q.amount = a;
                q.sale = None;
                q.code = Some(c.to_string());
            }
        }
        Ok(q)
    }

    /// The price a subscription was bought at (checkouts from before prices could change
    /// have none: they pay the plan's default).
    pub fn sub_price(&self, sub: &str) -> rusqlite::Result<Option<SubPrice>> {
        self.c()
            .query_row("SELECT amount, list_amount, code, sale_id FROM pro_sub_prices WHERE sub_id = ?1", [sub], |r| {
                Ok(SubPrice { amount: r.get(0)?, list_amount: r.get(1)?, code: r.get(2)?, sale_id: r.get(3)? })
            })
            .optional()
    }

    pub fn put_sub_price(&self, sub: &str, q: &Quote, plan_id: &str, now: i64) -> rusqlite::Result<()> {
        self.c().execute(
            "INSERT OR REPLACE INTO pro_sub_prices(sub_id, period, amount, list_amount, plan_id, code, sale_id, at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![sub, q.plan.key(), q.amount, q.list, plan_id, q.code, q.sale, now],
        )?;
        Ok(())
    }
}

/// The price stored with a subscription at checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubPrice {
    pub amount: i64,
    pub list_amount: i64,
    pub code: Option<String>,
    pub sale_id: Option<String>,
}

/// The prices the app and the website show: `{currency, monthly, yearly}` as a buyer pays
/// them now (the sale applied), the list prices, and the sale (or `null`).
pub fn public_prices(db: &Db, now: i64) -> rusqlite::Result<Value> {
    let lists = db.list_prices()?;
    let sale = db.live_sale(now)?;
    let mut prices = json!({ "currency": "INR" });
    let mut was = serde_json::Map::new();
    for p in Plan::ALL {
        let list = lists.get(&key(p)).copied().unwrap_or(p.amount_paise());
        let now_price = sale.as_ref().and_then(|s| s.amount(p, list));
        prices[p.key()] = json!(now_price.unwrap_or(list));
        if now_price.is_some() {
            was.insert(p.key().into(), json!(list));
        }
    }
    let sale = sale.filter(|_| !was.is_empty()).map(|s| {
        json!({
            "id": s.id,
            "name": s.name,
            "headline": s.headline,
            "message": s.message,
            "ends_at": s.ends_at,
            "show_countdown": s.show_countdown,
            "show_banner": s.show_banner,
            "theme": s.theme,
            "percent_off": s.percent_off(&lists),
            "was": Value::Object(was),
        })
    });
    Ok(json!({
        "prices": prices,
        "list_prices": { "currency": "INR", "monthly": lists.get("pro_monthly"), "yearly": lists.get("pro_yearly") },
        "sale": sale,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::now_ms;

    fn db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("relay.db")).unwrap();
        (dir, db)
    }

    fn sale(id: &str, starts: i64, ends: Option<i64>, monthly_pct: i64) -> SaleRow {
        SaleRow {
            id: id.into(),
            name: "Diwali".into(),
            headline: "30% off Pro".into(),
            message: None,
            discounts: BTreeMap::from([("pro_monthly".into(), monthly_pct)]),
            prices: BTreeMap::new(),
            starts_at: starts,
            ends_at: ends,
            stopped_at: None,
            show_countdown: true,
            show_banner: false,
            theme: "festive".into(),
            created_by: "o@x.co".into(),
            updated_by: "o@x.co".into(),
            created_at: starts,
            updated_at: starts,
        }
    }

    #[test]
    fn discounts_round_down_to_whole_rupees_and_never_below_one() {
        assert_eq!(discounted(19_900, 30), 13_900, "₹139.30 → ₹139");
        assert_eq!(discounted(199_900, 25), 149_900, "₹1,499.25 → ₹1,499");
        assert_eq!(discounted(19_900, 0), 19_900);
        assert_eq!(discounted(19_900, 100), MIN_AMOUNT);
        assert!(amount_ok(100) && amount_ok(1_499_999));
        assert!(!amount_ok(99) && !amount_ok(1_500_000));
    }

    #[test]
    fn prices_default_until_changed_and_keep_their_history() {
        let (_d, db) = db();
        assert_eq!(db.list_price(Plan::Monthly).unwrap(), (19_900, None));
        assert_eq!(db.list_price(Plan::Yearly).unwrap().0, 199_900);
        let r = db.set_price(Plan::Monthly, 24_900, "o@x.co", 1).unwrap();
        db.set_price(Plan::Monthly, 29_900, "o@x.co", 2).unwrap();
        assert_eq!(db.list_price(Plan::Monthly).unwrap().0, 29_900);
        assert_eq!(db.list_price(Plan::Yearly).unwrap().0, 199_900, "the other plan is untouched");
        let h = db.price_history().unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h.iter().find(|p| p.id == r.id).unwrap().retired_at, Some(2));
    }

    #[test]
    fn a_live_sale_lowers_the_quote_and_a_better_code_beats_it() {
        let (_d, db) = db();
        let now = now_ms();
        db.put_sale(&sale("sale_a", now - 1000, Some(now + 60_000), 30)).unwrap();
        let q = db.quote(Plan::Monthly, now, None).unwrap();
        assert_eq!((q.list, q.amount, q.sale.as_deref()), (19_900, 13_900, Some("sale_a")));
        assert_eq!(db.quote(Plan::Yearly, now, None).unwrap().amount, 199_900, "not on sale");
        let worse = db.quote(Plan::Monthly, now, Some(("TEN", 10))).unwrap();
        assert_eq!((worse.amount, worse.code), (13_900, None), "the sale is cheaper");
        let better = db.quote(Plan::Monthly, now, Some(("HALF", 50))).unwrap();
        assert_eq!((better.amount, better.code.as_deref(), better.sale), (9_900, Some("HALF"), None));
        // Before it starts and after it ends: list price.
        assert_eq!(db.quote(Plan::Monthly, now - 5000, None).unwrap().amount, 19_900);
        assert_eq!(db.quote(Plan::Monthly, now + 60_000, None).unwrap().amount, 19_900);
    }

    #[test]
    fn public_prices_show_the_sale_and_the_old_price() {
        let (_d, db) = db();
        let now = now_ms();
        let none = public_prices(&db, now).unwrap();
        assert_eq!(none["prices"], json!({ "currency": "INR", "monthly": 19_900, "yearly": 199_900 }));
        assert_eq!(none["sale"], Value::Null);
        let mut s = sale("sale_b", now - 1, Some(now + 1000), 30);
        s.prices.insert("pro_yearly".into(), 149_900);
        db.put_sale(&s).unwrap();
        let on = public_prices(&db, now).unwrap();
        assert_eq!(on["prices"], json!({ "currency": "INR", "monthly": 13_900, "yearly": 149_900 }));
        assert_eq!(on["sale"]["was"], json!({ "monthly": 19_900, "yearly": 199_900 }));
        assert_eq!(on["sale"]["ends_at"], now + 1000);
        assert_eq!(on["sale"]["percent_off"], 30);
        assert_eq!(on["list_prices"]["monthly"], 19_900);
        // A set price above the list price isn't a sale.
        let mut odd = sale("sale_c", now, None, 0);
        odd.discounts.clear();
        odd.prices.insert("pro_monthly".into(), 50_000);
        assert_eq!(odd.amount(Plan::Monthly, 19_900), None);
        assert_eq!(odd.state(now), "live");
        odd.stopped_at = Some(now);
        assert_eq!(odd.state(now), "stopped");
    }
}
