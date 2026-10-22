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
    render_with(r, unsubscribe_url, None)
}

/// One report, with a button to the web dashboard's History when there's one to open.
pub fn render_with(r: &Report, unsubscribe_url: &str, history_url: Option<&str>) -> Rendered {
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
        "\nFerrocast Pro sends this once a week from your stream history. Open Ferrocast › History for every stream.\nStop these emails: {unsubscribe_url}\n{}\n",
        super::layout::POSTAL_ADDRESS
    ));

    // HTML: the shared layout (design board 2026-10-22-polish). The four main numbers are
    // tiles with their change against last week; the rest are plain rows.
    use super::layout::{self as l, FAINT, FONT, INK, LINE, MUTED, SOFT};
    let tile = |(k, v, d): &(&str, String, Option<String>)| {
        let d = d
            .as_ref()
            .map(|d| {
                let (colour, mark) = if d.starts_with("up ") {
                    (l::UP, "&#9650; ")
                } else if d.starts_with("down ") {
                    (l::DOWN, "&#9660; ")
                } else {
                    (FAINT, "")
                };
                format!(
                    "<div style=\"{FONT};font-size:12.5px;line-height:18px;font-weight:600;color:{colour}\">{mark}{}</div>",
                    esc(d)
                )
            })
            .unwrap_or_default();
        format!(
            "<td width=\"50%\" valign=\"top\" style=\"padding:5px\"><div style=\"border:1px solid {LINE};border-radius:12px;padding:14px\">\
             <div style=\"{FONT};font-size:12.5px;line-height:18px;color:{FAINT}\">{k}</div>\
             <div style=\"{FONT};font-size:26px;line-height:32px;font-weight:800;letter-spacing:-.02em;color:{INK}\">{}</div>{d}</div></td>",
            esc(v)
        )
    };
    let (main, rest) = lines.split_at(lines.len().min(4));
    let mut tiles = String::new();
    for pair in main.chunks(2) {
        tiles.push_str("<tr>");
        for t in pair {
            tiles.push_str(&tile(t));
        }
        if pair.len() == 1 {
            tiles.push_str("<td width=\"50%\"></td>");
        }
        tiles.push_str("</tr>");
    }
    let mut extras = String::new();
    for (k, v, d) in rest {
        let d = d
            .as_ref()
            .map(|d| format!("<div style=\"{FONT};font-size:12.5px;line-height:18px;color:{FAINT}\">{}</div>", esc(d)))
            .unwrap_or_default();
        extras.push_str(&format!(
            "<tr><td style=\"padding:10px 0;border-bottom:1px solid {LINE};{FONT};font-size:14.5px;line-height:20px;color:{MUTED}\">{k}</td>\
             <td align=\"right\" style=\"padding:10px 0;border-bottom:1px solid {LINE};text-align:right;{FONT};font-size:14.5px;line-height:20px;font-weight:700;color:{INK}\">{}{d}</td></tr>",
            esc(v)
        ));
    }
    let extras = if extras.is_empty() {
        String::new()
    } else {
        format!(
            "<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" style=\"margin:0 0 22px;border-top:1px solid {LINE}\">{extras}</table>"
        )
    };
    let compare = if r.previous.is_none() {
        l::para("No streams the week before, so nothing to compare yet.")
    } else {
        String::new()
    };
    let best = r
        .best
        .as_ref()
        .map(|b| {
            format!(
                "<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" style=\"margin:0 0 22px\"><tr>\
                 <td style=\"padding:16px;border-left:3px solid {m};border-radius:12px;background:{SOFT}\">\
                 <div style=\"{FONT};font-size:12px;line-height:16px;font-weight:700;letter-spacing:.06em;text-transform:uppercase;color:{FAINT}\">Best stream</div>\
                 <div style=\"margin:4px 0 2px;{FONT};font-size:17px;line-height:24px;font-weight:700;color:{INK}\">{}</div>\
                 <div style=\"{FONT};font-size:14px;line-height:20px;color:{MUTED}\">{} · {} · {}<br>{} average viewers, peak {}</div>\
                 </td></tr></table>",
                esc(b.title.as_deref().unwrap_or("Untitled stream")),
                platform_name(&b.platform),
                esc(&date(b.started_at)),
                esc(&length(b.minutes)),
                count(b.avg_viewers),
                count(b.peak_viewers),
                m = l::MOLTEN,
            )
        })
        .unwrap_or_default();
    let button = history_url.map(|u| l::button(u, "Open History")).unwrap_or_default();
    let body = format!(
        "{}{}\
         <table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" style=\"margin:6px 0 17px\">{tiles}</table>\
         {compare}{extras}{best}{button}",
        l::eyebrow(&format!("Your week · {when} (UTC)")),
        l::title(&headline),
    );
    let footer = format!(
        "Ferrocast Pro sends this once a week from your stream history.<br>{} · {}",
        l::footer_link(unsubscribe_url, "Stop these emails"),
        l::footer_link("https://ferrocast.clearcraftlabs.com/contact/", "Help"),
    );
    let html = l::page(&subject, &format!("{headline}."), &body, &footer);
    Rendered { subject, html, text }
}

/// The double opt-in email.
pub fn confirm(link: &str) -> Rendered {
    use super::layout as l;
    let subject = "Confirm your Ferrocast weekly email".to_string();
    let text = format!(
        "Confirm your weekly email\n\nOpen this link and press Confirm to get a short report of your streams once a week:\n{link}\n\n\
         The link works for 24 hours. If you didn't ask for this, ignore this email: nothing is sent without the confirm.\n\n{}\n",
        super::layout::POSTAL_ADDRESS
    );
    let body = format!(
        "{}{}{}{}{}",
        l::eyebrow("Weekly report"),
        l::title("Confirm your weekly email"),
        l::para(
            "Once a week you'll get a short report of your streams: hours live, viewers, followers and your best stream."
        ),
        l::button(link, "Confirm my email"),
        l::note(
            "The link works for 24 hours. Didn't ask for this? Ignore this email. Nothing is sent until you confirm."
        ),
    );
    let html = l::page(
        &subject,
        "Press the button to get a short report of your streams once a week.",
        &body,
        &l::footer("Ferrocast by ClearCraft Labs"),
    );
    Rendered { subject, html, text }
}
