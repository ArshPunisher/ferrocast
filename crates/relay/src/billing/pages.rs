//! The relay's two small Pro pages: manage (status, cancel) and plain messages. Plain
//! HTML, no scripts, nothing from other sites; every value is escaped.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};

use super::access::{self, Plan};
use super::store::SubRow;
use crate::{AppState, now_ms};

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// `18 Oct 2026` (UTC) from Unix milliseconds.
pub fn date(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let month = usize::try_from(m - 1).ok().and_then(|i| MONTHS.get(i)).unwrap_or(&"");
    format!("{d} {month} {y}")
}

fn rupees(paise: i64) -> String {
    let r = paise / 100;
    // Indian grouping is the same as plain grouping below ₹1,00,000.
    if r >= 1000 { format!("₹{},{:03}", r / 1000, r % 1000) } else { format!("₹{r}") }
}

fn shell(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <meta name=robots content=noindex><title>{t} · Ferrocast</title>\
         <style>:root{{color-scheme:dark light;--bg:#0c0e12;--card:#15181f;--line:#2a2f3a;--text:#ecedf1;--muted:#a3a8b5;--accent:#ff5a1f}}\
         @media (prefers-color-scheme:light){{:root{{--bg:#f6f6f4;--card:#fff;--line:#dedfe3;--text:#14161b;--muted:#5b606c}}}}\
         body{{margin:0;min-height:100vh;display:grid;place-items:center;padding:16px;background:var(--bg);color:var(--text);font:16px/1.5 system-ui,sans-serif}}\
         main{{width:100%;max-width:520px;background:var(--card);border:1px solid var(--line);border-radius:16px;padding:28px}}\
         h1{{font-size:1.4rem;margin:0 0 12px}}dl{{display:grid;grid-template-columns:auto 1fr;gap:6px 16px;margin:20px 0}}\
         dt{{color:var(--muted)}}dd{{margin:0;overflow-wrap:anywhere}}p{{margin:0 0 12px}}.muted{{color:var(--muted);font-size:.9rem}}\
         .ok{{border-left:3px solid #3fb950;padding-left:12px}}.bad{{border-left:3px solid #f85149;padding-left:12px}}\
         button{{font:inherit;font-weight:600;min-height:44px;padding:0 18px;border-radius:10px;border:1px solid var(--line);background:transparent;color:var(--text);cursor:pointer}}\
         button:focus-visible{{outline:2px solid var(--accent);outline-offset:2px}}</style>\
         <main><h1>{t}</h1>{body}</main></html>",
        t = esc(title),
    );
    let mut r = (status, Html(html)).into_response();
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    // The link carries the page's token: never send it on as a referrer.
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'",
        ),
    );
    r
}

pub fn message(status: StatusCode, title: &str, text: &str) -> Response {
    shell(status, title, &format!("<p>{}</p>", esc(text)))
}

/// What the subscription means, in words.
fn summary(s: &SubRow) -> String {
    let a = s.access();
    let until = access::access_until(&a).map(date).unwrap_or_default();
    let end = s.current_end.map(date).unwrap_or_default();
    if s.refunded_at.is_some() {
        return "Refunded. Pro has ended.".into();
    }
    match s.status.as_str() {
        "active" if s.cancel_at_end => format!("Active until {end}. It won't renew."),
        "active" => format!("Active. Renews on {end}."),
        "pending" => format!("Razorpay is retrying your renewal payment. Pro stays on until {until}."),
        "halted" => "Paused: every renewal attempt failed. Subscribe again from Ferrocast to get Pro back.".into(),
        "paused" => "Paused.".into(),
        "authenticated" => "Payment method set up. Pro starts with the first charge.".into(),
        "created" => "Checkout isn't finished yet.".into(),
        "cancelled" | "completed" if access::is_pro(&a, now_ms()) => format!("Ended. Pro stays on until {until}."),
        "cancelled" | "completed" => "Ended.".into(),
        _ => "Expired.".into(),
    }
}

pub fn manage(st: &AppState, s: &SubRow, token: &str, note: Option<Result<&str, String>>) -> Response {
    let plan = Plan::parse(&s.plan);
    let price = plan
        .map(|p| format!("{} a {}", rupees(p.amount_paise()), if p == Plan::Monthly { "month" } else { "year" }))
        .unwrap_or_default();
    let installs = st.db.pro_installs(&s.id).unwrap_or(0);
    let note = match note {
        Some(Ok(t)) => format!("<p class=ok role=status>{}</p>", esc(t)),
        Some(Err(t)) => format!("<p class=bad role=alert>{}</p>", esc(&t)),
        None => String::new(),
    };
    let can_cancel = access::BLOCKING.contains(&s.status.as_str()) && !s.cancel_at_end && s.refunded_at.is_none();
    let cancel = if can_cancel {
        format!(
            "<form method=post action=\"/pro/manage/{}/cancel\"><p>Stop renewing? Pro stays on until the end of the period you paid for.</p>\
             <button type=submit>Stop renewing</button></form>",
            esc(token)
        )
    } else {
        String::new()
    };
    let body = format!(
        "{note}<p>{}</p><dl><dt>Plan</dt><dd>{}</dd><dt>Subscription</dt><dd>{}</dd><dt>Installs</dt><dd>{installs} of {}</dd></dl>{cancel}\
         <p class=muted>Payments are handled by Razorpay; Ferrocast never sees your card or UPI details. \
         To move Pro to a new PC, use Settings › Pro › Restore there with this subscription id and your checkout email.</p>",
        esc(&summary(s)),
        esc(&price),
        esc(&s.id),
        super::store::MAX_INSTALLS,
    );
    shell(StatusCode::OK, "Ferrocast Pro", &body)
}
