//! The weekly report: built from the stored stream history (`crate::history`), rendered as
//! HTML (inline styles, tables, light, readable when Gmail's dark mode inverts it) and as
//! plain text. Every stream title is escaped: titles come from the platform and can hold
//! anything.

use std::collections::BTreeMap;

use crate::billing::date;
use crate::history::StreamOut;

/// Totals of one week of streams.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Totals {
    /// One per platform when simulcasting, as in the history.
    pub streams: i64,
    pub minutes: i64,
    pub peak_viewers: i64,
    /// Weighted by each stream's length (as `history::weekly`).
    pub avg_viewers: i64,
    pub follows: i64,
    pub subs: i64,
    /// Per currency, minor units, never converted (R-32).
    pub tips: BTreeMap<String, i64>,
}

/// The week's best stream: the most average viewers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Best {
    pub title: Option<String>,
    pub platform: String,
    pub started_at: i64,
    pub minutes: i64,
    pub avg_viewers: i64,
    pub peak_viewers: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// `[from, to)`, the seven days reported.
    pub from: i64,
    pub to: i64,
    pub week: Totals,
    /// The seven days before; `None` when there were no streams then.
    pub previous: Option<Totals>,
    pub best: Option<Best>,
}

fn minutes(s: &StreamOut, to: i64) -> i64 {
    (s.ended_at.unwrap_or(to).min(to) - s.started_at).max(0) / 60_000
}

pub fn totals(streams: &[StreamOut], to: i64) -> Totals {
    let mut t = Totals::default();
    let mut weighted: i128 = 0;
    for s in streams {
        let m = minutes(s, to);
        t.streams += 1;
        t.minutes += m;
        t.peak_viewers = t.peak_viewers.max(s.peak_viewers);
        t.follows += s.follows;
        t.subs += s.subs;
        for tip in &s.tips {
            *t.tips.entry(tip.currency.clone()).or_default() += tip.minor;
        }
        weighted += i128::from(s.avg_viewers) * i128::from(m.max(1));
    }
    let total = i128::from(t.minutes.max(t.streams));
    if total > 0 {
        t.avg_viewers = i64::try_from((weighted + total / 2) / total).unwrap_or(0);
    }
    t
}

/// The report for `[from, to)`, or `None` without a stream in it (that week isn't sent).
pub fn build(week: &[StreamOut], previous: &[StreamOut], from: i64, to: i64) -> Option<Report> {
    if week.is_empty() {
        return None;
    }
    let best =
        week.iter().max_by_key(|s| (s.avg_viewers, s.peak_viewers, minutes(s, to), -s.started_at)).map(|s| Best {
            title: s.title.clone(),
            platform: s.platform.clone(),
            started_at: s.started_at,
            minutes: minutes(s, to),
            avg_viewers: s.avg_viewers,
            peak_viewers: s.peak_viewers,
        });
    Some(Report {
        from,
        to,
        week: totals(week, to),
        previous: (!previous.is_empty()).then(|| totals(previous, from)),
        best,
    })
}

// ── Formatting ───────────────────────────────────────────────────────────────────────

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// `12,345`.
pub fn count(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// `14.5` hours from minutes.
pub fn hours(minutes: i64) -> String {
    let tenths = (minutes * 10 + 30) / 60;
    if tenths % 10 == 0 { count(tenths / 10) } else { format!("{}.{}", count(tenths / 10), tenths % 10) }
}

/// `2 h 14 min`.
fn length(minutes: i64) -> String {
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

const NO_DECIMALS: [&str; 6] = ["JPY", "KRW", "VND", "CLP", "ISK", "UGX"];

/// `INR 1,250.00`.
pub fn money(currency: &str, minor: i64) -> String {
    if NO_DECIMALS.contains(&currency) {
        format!("{currency} {}", count(minor))
    } else {
        format!("{currency} {}.{:02}", count(minor / 100), (minor % 100).abs())
    }
}

fn platform_name(p: &str) -> &'static str {
    match p {
        "twitch" => "Twitch",
        "youtube" => "YouTube",
        "kick" => "Kick",
        _ => "",
    }
}

/// `12-18 Oct 2026` for `[from, to)`.
pub fn span(from: i64, to: i64) -> String {
    let a = date(from);
    let b = date(to - 1);
    let (ad, arest) = a.split_once(' ').unwrap_or((&a, ""));
    if arest == b.split_once(' ').map_or("", |x| x.1) { format!("{ad}-{b}") } else { format!("{a} to {b}") }
}

/// "+12% vs last week", "same as last week", or `None` without a previous week.
pub fn delta(now: i64, before: i64) -> Option<String> {
    if before <= 0 {
        return (now > 0).then(|| "none the week before".to_string());
    }
    let pct = ((now - before) as f64 / before as f64 * 100.0).round() as i64;
    Some(match pct {
        0 => "same as last week".into(),
        p if p > 0 => format!("up {p}% on last week"),
        p => format!("down {}% on last week", -p),
    })
}

/// Subject, HTML and text of one report.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub subject: String,
    pub html: String,
    pub text: String,
}

/// What the numbers say, as label, value, and the comparison.
fn lines(r: &Report) -> Vec<(&'static str, String, Option<String>)> {
    let w = &r.week;
    let p = r.previous.as_ref();
    let cmp = |f: fn(&Totals) -> i64| p.and_then(|p| delta(f(w), f(p)));
    let mut v = vec![
        ("Hours live", hours(w.minutes), cmp(|t| t.minutes)),
        ("Average viewers", count(w.avg_viewers), cmp(|t| t.avg_viewers)),
        ("Peak viewers", count(w.peak_viewers), cmp(|t| t.peak_viewers)),
        ("Streams", count(w.streams), cmp(|t| t.streams)),
    ];
    if w.follows > 0 || p.is_some_and(|p| p.follows > 0) {
        v.push(("New followers", count(w.follows), cmp(|t| t.follows)));
    }
    if w.subs > 0 || p.is_some_and(|p| p.subs > 0) {
        v.push(("New subs", count(w.subs), cmp(|t| t.subs)));
    }
    if !w.tips.is_empty() {
        let tips = w.tips.iter().map(|(c, m)| money(c, *m)).collect::<Vec<_>>().join(", ");
        v.push(("Tips", tips, None));
    }
    v
}

pub fn render(r: &Report, unsubscribe_url: &str) -> Rendered {
    let when = span(r.from, r.to);
    let w = &r.week;
    let subject = format!(
        "Your week on stream: {} {}, {} hours",
        count(w.streams),
        if w.streams == 1 { "stream" } else { "streams" },
        hours(w.minutes)
    );
    let headline = format!("{} hours live, {} average viewers", hours(w.minutes), count(w.avg_viewers));
    let lines = lines(r);

    // Plain text.
    let mut text = format!("Your week on stream, {when} (UTC)\n\n{headline}.\n\n");
    for (k, v, d) in &lines {
        text.push_str(&format!("{k}: {v}"));
        if let Some(d) = d {
            text.push_str(&format!(" ({d})"));
        }
        text.push('\n');
    }
    if r.previous.is_none() {
        text.push_str("\nNo streams the week before, so nothing to compare yet.\n");
    }
    if let Some(b) = &r.best {
        text.push_str(&format!(
            "\nBest stream: {} on {}, {} ({}), {} average viewers, peak {}.\n",
            b.title.as_deref().unwrap_or("Untitled stream"),
            platform_name(&b.platform),
            date(b.started_at),
            length(b.minutes),
            count(b.avg_viewers),
            count(b.peak_viewers),
        ));
    }
    text.push_str(&format!(
        "\nFerrocast Pro sends this once a week from your stream history. Open Ferrocast › History for every stream.\nStop these emails: {unsubscribe_url}\n"
    ));

    // HTML: tables and inline styles only (mail apps drop <style> and scripts), one column,
    // solid colours that still read when a dark mode inverts them.
    const INK: &str = "#14161b";
    const MUTED: &str = "#4a505b";
    const LINE: &str = "#dfe2e8";
    const ACCENT: &str = "#8a4b00";
    let font = "font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif";
    let mut rows = String::new();
    for (k, v, d) in &lines {
        let d = d
            .as_ref()
            .map(|d| format!("<div style=\"{font};font-size:13px;line-height:18px;color:{MUTED}\">{}</div>", esc(d)))
            .unwrap_or_default();
        rows.push_str(&format!(
            "<tr><td style=\"padding:12px 0;border-top:1px solid {LINE};{font};font-size:15px;line-height:20px;color:{MUTED}\">{k}</td>\
             <td align=\"right\" style=\"padding:12px 0;border-top:1px solid {LINE};text-align:right\">\
             <div style=\"{font};font-size:20px;line-height:26px;font-weight:700;color:{INK}\">{}</div>{d}</td></tr>",
            esc(v)
        ));
    }
    let compare = if r.previous.is_none() {
        format!(
            "<p style=\"margin:16px 0 0;{font};font-size:14px;line-height:20px;color:{MUTED}\">No streams the week before, so nothing to compare yet.</p>"
        )
    } else {
        String::new()
    };
    let best = r
        .best
        .as_ref()
        .map(|b| {
            format!(
                "<h2 style=\"margin:28px 0 8px;{font};font-size:16px;line-height:22px;font-weight:700;color:{INK}\">Best stream</h2>\
                 <p style=\"margin:0;{font};font-size:17px;line-height:24px;font-weight:600;color:{INK}\">{}</p>\
                 <p style=\"margin:4px 0 0;{font};font-size:14px;line-height:20px;color:{MUTED}\">{} · {} · {}<br>{} average viewers, peak {}</p>",
                esc(b.title.as_deref().unwrap_or("Untitled stream")),
                platform_name(&b.platform),
                esc(&date(b.started_at)),
                esc(&length(b.minutes)),
                count(b.avg_viewers),
                count(b.peak_viewers),
            )
        })
        .unwrap_or_default();
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <meta name=\"color-scheme\" content=\"light\"><meta name=\"supported-color-schemes\" content=\"light\">\
         <title>{subject_e}</title></head>\
         <body style=\"margin:0;padding:0;background:#f2f3f6\">\
         <div style=\"display:none;max-height:0;overflow:hidden;color:#f2f3f6\">{headline_e}.</div>\
         <table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" style=\"background:#f2f3f6\"><tr><td align=\"center\" style=\"padding:24px 12px\">\
         <table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" style=\"max-width:560px;background:#ffffff;border:1px solid {LINE};border-radius:16px\">\
         <tr><td style=\"padding:28px 24px\">\
         <p style=\"margin:0 0 4px;{font};font-size:13px;line-height:18px;font-weight:700;letter-spacing:.06em;text-transform:uppercase;color:{ACCENT}\">Ferrocast · your week</p>\
         <p style=\"margin:0 0 16px;{font};font-size:14px;line-height:20px;color:{MUTED}\">{when_e} (UTC)</p>\
         <h1 style=\"margin:0 0 20px;{font};font-size:26px;line-height:32px;font-weight:800;color:{INK}\">{headline_e}</h1>\
         <table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\">{rows}</table>\
         {compare}{best}\
         </td></tr></table>\
         <p style=\"margin:16px 0 0;max-width:560px;{font};font-size:13px;line-height:19px;color:{MUTED}\">\
         Ferrocast Pro sends this once a week from your stream history. Open Ferrocast › History for every stream.<br>\
         <a href=\"{unsub_e}\" style=\"color:{ACCENT};text-decoration:underline\">Stop these emails</a></p>\
         </td></tr></table></body></html>",
        subject_e = esc(&subject),
        headline_e = esc(&headline),
        when_e = esc(&when),
        unsub_e = esc(unsubscribe_url),
    );
    Rendered { subject, html, text }
}

/// The double opt-in email.
pub fn confirm(link: &str) -> Rendered {
    let subject = "Confirm your Ferrocast weekly email".to_string();
    let font = "font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif";
    let text = format!(
        "Confirm your weekly email\n\nOpen this link and press Confirm to get a short report of your streams once a week:\n{link}\n\n\
         The link works for 24 hours. If you didn't ask for this, ignore this email: nothing is sent without the confirm.\n"
    );
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <meta name=\"color-scheme\" content=\"light\"><title>{subject}</title></head>\
         <body style=\"margin:0;padding:0;background:#f2f3f6\">\
         <table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" style=\"background:#f2f3f6\"><tr><td align=\"center\" style=\"padding:24px 12px\">\
         <table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" style=\"max-width:520px;background:#ffffff;border:1px solid #dfe2e8;border-radius:16px\">\
         <tr><td style=\"padding:28px 24px\">\
         <h1 style=\"margin:0 0 12px;{font};font-size:24px;line-height:30px;font-weight:800;color:#14161b\">Confirm your weekly email</h1>\
         <p style=\"margin:0 0 20px;{font};font-size:16px;line-height:24px;color:#14161b\">Press the button, then Confirm, to get a short report of your streams once a week.</p>\
         <a href=\"{link_e}\" style=\"display:inline-block;padding:14px 22px;border-radius:10px;background:#cf7400;color:#0a0600;{font};font-size:16px;font-weight:700;text-decoration:none\">Confirm my email</a>\
         <p style=\"margin:20px 0 0;{font};font-size:13px;line-height:19px;color:#4a505b\">The link works for 24 hours. Didn't ask for this? Ignore this email: nothing is sent without the confirm.</p>\
         </td></tr></table></td></tr></table></body></html>",
        link_e = esc(link),
    );
    Rendered { subject, html, text }
}
