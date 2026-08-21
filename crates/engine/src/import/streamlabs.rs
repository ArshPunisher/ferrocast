//! Streamlabs: from files the streamer exports, because Streamlabs offers no way to read
//! a streamer's bot setup without an app registered with Streamlabs.
//!
//! - **Points**: the Currency export of Streamlabs Chatbot (CSV or Excel `.xlsx`, columns
//!   like `Name, Rank, Points, Hours`), or any sheet with a name column and a points column.
//! - **Tip history**: a donations CSV/XLSX with date, name, amount and currency columns
//!   (message and id are optional).
//!
//! Cloudbot commands and timers can't be exported from Streamlabs and Chatbot's `.abcomg`
//! command files are a private format, so they aren't read (the report says so).
//!
//! Files are checked strictly: over [`MAX_FILE`] is 413, anything that isn't CSV or `.xlsx`
//! is 415, and a missing column or a bad value is 422 naming the file, row and column.

use ferrocast_core::event::Platform;
use ferrocast_core::loyalty::MAX_BALANCE;
use ferrocast_store::import::{Incoming, Kind, NotImported, PointsRow, Source, TipRow};

use super::refused;
use super::text::{parse_datetime, stable_id};
use super::xlsx::{self, XlsxError};
use crate::EngineError;

/// Largest file of each kind.
pub const MAX_FILE: usize = 10 * 1024 * 1024;
/// Most rows read from one file.
pub const MAX_ROWS: usize = 200_000;

/// One uploaded file.
#[derive(Debug, Clone)]
pub struct Upload {
    pub file_name: String,
    pub bytes: Vec<u8>,
}

/// What the streamer sent.
#[derive(Debug, Clone, Default)]
pub struct Files {
    /// Whose viewers the points belong to.
    pub platform: Option<Platform>,
    pub points: Option<Upload>,
    pub tips: Option<Upload>,
}

/// Which file a problem is in, for messages ("points file, row 14").
#[derive(Debug, Clone, Copy)]
enum Which {
    Points,
    Tips,
}

impl Which {
    fn label(self) -> &'static str {
        match self {
            Which::Points => "points file",
            Which::Tips => "tips file",
        }
    }
}

pub fn too_large(which: &str) -> EngineError {
    refused(
        413,
        "too_large",
        format!("The {which} is over {} MB, the most Ferrocast reads.", MAX_FILE / 1024 / 1024),
        "Export only the columns you need, or split the file, then try again.",
    )
}

fn wrong_type(which: Which, what: &str) -> EngineError {
    refused(
        415,
        "unsupported_type",
        format!("The {} {what}. Use a CSV file or an Excel .xlsx workbook.", which.label()),
        "In Excel or Google Sheets, use File → Save as (or Download) → CSV, then pick that file.",
    )
}

fn malformed(which: Which, message: String, hint: &str) -> EngineError {
    refused(422, "bad_file", format!("{}: {message}", which.label()), hint)
}

const COLUMN_HINT: &str = "Open the file and check its first row names the columns, like Name and Points.";

/// File bytes → rows of trimmed cells, with blank rows dropped.
fn table(which: Which, up: &Upload) -> Result<Vec<Vec<String>>, EngineError> {
    let b = &up.bytes;
    if b.len() > MAX_FILE {
        return Err(too_large(which.label()));
    }
    if b.is_empty() {
        return Err(malformed(which, "the file is empty.".into(), "Export it again from Streamlabs, then pick it."));
    }
    let rows = if b.starts_with(b"PK\x03\x04") {
        xlsx::rows(b).map_err(|e| match e {
            XlsxError::NotAWorkbook => wrong_type(which, "is a zip file, not an Excel workbook"),
            XlsxError::Damaged(why) => malformed(
                which,
                format!("the workbook can't be read: {why}."),
                "Open it in Excel or Google Sheets and save it again, or save it as CSV.",
            ),
        })?
    } else if b.starts_with(&[0xD0, 0xCF, 0x11, 0xE0]) {
        return Err(wrong_type(which, "is an old Excel .xls file, which Ferrocast can't read"));
    } else {
        csv(&decode(which, b)?)
    };
    let rows: Vec<Vec<String>> = rows
        .into_iter()
        .map(|r| r.into_iter().map(|c| c.trim().to_string()).collect::<Vec<_>>())
        .filter(|r| r.iter().any(|c| !c.is_empty()))
        .collect();
    if rows.len() > MAX_ROWS + 1 {
        return Err(refused(
            413,
            "too_large",
            format!("The {} has more than {MAX_ROWS} rows, the most Ferrocast reads.", which.label()),
            "Split it into smaller files and import them one after another.",
        ));
    }
    Ok(rows)
}

/// UTF-8 (with or without BOM) or UTF-16 with a BOM ("Unicode text" from Excel).
fn decode(which: Which, b: &[u8]) -> Result<String, EngineError> {
    let not_text = || wrong_type(which, "isn't a text or spreadsheet file");
    let text = if let Some(rest) = b.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        String::from_utf8(rest.to_vec()).map_err(|_| not_text())?
    } else if b.starts_with(&[0xFF, 0xFE]) || b.starts_with(&[0xFE, 0xFF]) {
        let le = b[0] == 0xFF;
        let units: Vec<u16> = b[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| if le { u16::from_le_bytes(*c) } else { u16::from_be_bytes(*c) })
            .collect();
        String::from_utf16(&units).map_err(|_| not_text())?
    } else {
        String::from_utf8(b.to_vec()).map_err(|_| not_text())?
    };
    if text.contains('\0')
        || text.starts_with("%PDF")
        || text.trim_start().starts_with('<')
        || text.trim_start().starts_with('{')
    {
        return Err(not_text());
    }
    Ok(text)
}

/// RFC 4180 CSV with the delimiter guessed from the first line (`,`, `;` or tab: Excel
/// uses `;` in many countries). An Excel `sep=;` first line is honoured.
fn csv(text: &str) -> Vec<Vec<String>> {
    let mut text = text;
    let mut delim = None;
    if let Some(first) = text.lines().next()
        && let Some(d) = first.trim().strip_prefix("sep=")
        && let Some(c) = d.chars().next()
    {
        delim = Some(c);
        text = text.split_once('\n').map_or("", |(_, rest)| rest);
    }
    let delim = delim.unwrap_or_else(|| {
        let first = text.lines().next().unwrap_or("");
        let mut best = (',', 0);
        for d in [',', ';', '\t'] {
            let n = first.matches(d).count();
            if n > best.1 {
                best = (d, n);
            }
        }
        best.0
    });
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    cell.push('"');
                    chars.next();
                } else {
                    quoted = false;
                }
            } else {
                cell.push(c);
            }
        } else if c == '"' && cell.trim().is_empty() {
            cell.clear();
            quoted = true;
        } else if c == delim {
            row.push(std::mem::take(&mut cell));
        } else if c == '\n' || c == '\r' {
            if c == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            row.push(std::mem::take(&mut cell));
            // blank lines (e.g. "\r\r\n" endings) must not count against the row cap
            let r = std::mem::take(&mut row);
            if r.iter().any(|c| !c.trim().is_empty()) {
                rows.push(r);
            }
        } else {
            cell.push(c);
        }
        if rows.len() > MAX_ROWS + 2 {
            break;
        }
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    rows
}

fn norm(h: &str) -> String {
    h.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase()
}

/// The first column whose header is one of `names` (compared without case, spaces or _).
fn find(header: &[String], names: &[&str]) -> Option<usize> {
    let header: Vec<String> = header.iter().map(|h| norm(h)).collect();
    names.iter().find_map(|n| header.iter().position(|h| h == n))
}

fn need(which: Which, header: &[String], what: &str, names: &[&str]) -> Result<usize, EngineError> {
    find(header, names).ok_or_else(|| {
        malformed(
            which,
            format!(
                "there's no column for {what}. The first row should name it, for example {}. Found: {}.",
                names.iter().take(3).map(|n| format!("\"{n}\"")).collect::<Vec<_>>().join(" or "),
                if header.is_empty() { "nothing".to_string() } else { header.join(", ") }
            ),
            COLUMN_HINT,
        )
    })
}

fn cell(row: &[String], col: usize) -> &str {
    row.get(col).map(String::as_str).unwrap_or("")
}

/// A whole number like `1500`, `1,500`, `1 500` or `1500.0` (spreadsheets add `.0`).
fn whole(s: &str) -> Option<i64> {
    let s: String = s.chars().filter(|c| !matches!(c, ',' | ' ' | '\u{a0}' | '\'' | '_')).collect();
    let s = s.strip_suffix(".0").or_else(|| s.strip_suffix(".00")).unwrap_or(&s);
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && s.len() <= 15).then(|| s.parse().ok()).flatten()
}

fn points_rows(inc: &mut Incoming, up: &Upload) -> Result<(), EngineError> {
    let w = Which::Points;
    let rows = table(w, up)?;
    let Some((header, body)) = rows.split_first() else {
        return Err(malformed(w, "the file has no rows.".into(), COLUMN_HINT));
    };
    let name_col =
        need(w, header, "the viewer's name", &["name", "username", "user", "viewer", "displayname", "login"])?;
    let points_col = need(w, header, "points", &["points", "currency", "balance", "loyaltypoints", "amount"])?;
    // Optional: the platform's user id (YouTube channel id, Twitch or Kick user id).
    let id_col = find(header, &["userid", "channelid", "providerid", "twitchid", "youtubeid", "kickid", "id"]);
    let label = |col: usize| header.get(col).cloned().unwrap_or_default();
    for (i, row) in body.iter().enumerate() {
        let line = i + 2;
        let name = cell(row, name_col);
        if name.is_empty() {
            return Err(malformed(
                w,
                format!("row {line} has no name in the \"{}\" column.", label(name_col)),
                "Fill it in or delete that row, then try again.",
            ));
        }
        let raw = cell(row, points_col);
        let Some(points) = whole(raw) else {
            return Err(malformed(
                w,
                format!(
                    "row {line}, column \"{}\": \"{raw}\" isn't a whole number of points (0 or more).",
                    label(points_col)
                ),
                "Fix that cell, then try again.",
            ));
        };
        if points > MAX_BALANCE {
            return Err(malformed(
                w,
                format!("row {line}, column \"{}\": {raw} is more points than anyone can hold.", label(points_col)),
                "Fix that cell, then try again.",
            ));
        }
        let user_id = id_col.map(|c| cell(row, c)).filter(|s| !s.is_empty()).map(|s| s.chars().take(64).collect());
        inc.points.push(PointsRow { name: name.chars().take(100).collect(), user_id, points });
    }
    Ok(())
}

/// An amount as a plain decimal ("12.50"), whichever separators the file uses: "$1,234.50",
/// "1.234,50 €" and "12,50" all work. The last `.` or `,` is the decimal mark when one or two
/// digits follow it, a thousands separator when three do; anything else is `None`.
fn decimal_amount(raw: &str) -> Option<String> {
    let s: String = raw.chars().filter(|c| c.is_ascii_digit() || matches!(c, '.' | ',')).collect();
    if s.is_empty() || !s.bytes().any(|b| b.is_ascii_digit()) {
        return None;
    }
    let Some(at) = s.rfind(['.', ',']) else { return Some(s) };
    let (whole, frac) = (&s[..at], &s[at + 1..]);
    let whole: String = whole.chars().filter(char::is_ascii_digit).collect();
    match frac.len() {
        1 | 2 => Some(format!("{}.{frac}", if whole.is_empty() { "0" } else { &whole })),
        3 => Some(format!("{whole}{frac}")),
        _ => None,
    }
}

/// Days since 1899-12-30 (how Excel stores dates) → Unix ms.
fn excel_serial(s: &str) -> Option<i64> {
    let v: f64 = s.parse().ok()?;
    (20_000.0..80_000.0).contains(&v).then(|| ((v - 25_569.0) * 86_400_000.0).round() as i64)
}

fn tip_rows(inc: &mut Incoming, up: &Upload) -> Result<(), EngineError> {
    let w = Which::Tips;
    let rows = table(w, up)?;
    let Some((header, body)) = rows.split_first() else {
        return Err(malformed(w, "the file has no rows.".into(), COLUMN_HINT));
    };
    let date_col = need(w, header, "the date", &["date", "createdat", "time", "timestamp", "datetime"])?;
    let name_col = need(w, header, "the tipper's name", &["name", "donor", "from", "username", "user", "donatorname"])?;
    let amount_col = need(w, header, "the amount", &["amount", "donation", "value", "total"])?;
    let currency_col = need(w, header, "the currency", &["currency", "currencycode"])?;
    let message_col = find(header, &["message", "note", "comment"]);
    let id_col = find(header, &["id", "transactionid", "donationid", "paymentid"]);
    let label = |col: usize| header.get(col).cloned().unwrap_or_default();
    for (i, row) in body.iter().enumerate() {
        let line = i + 2;
        let at_raw = cell(row, date_col);
        let Some(at) = parse_datetime(at_raw).or_else(|| excel_serial(at_raw)) else {
            return Err(malformed(
                w,
                format!("row {line}, column \"{}\": \"{at_raw}\" isn't a date Ferrocast can read.", label(date_col)),
                "Dates should look like 2024-05-01 or 2024-05-01 18:30:00.",
            ));
        };
        let currency = cell(row, currency_col).to_ascii_uppercase();
        if currency.len() != 3 || !currency.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err(malformed(
                w,
                format!(
                    "row {line}, column \"{}\": \"{currency}\" isn't a currency code like USD or INR.",
                    label(currency_col)
                ),
                "Fix that cell, then try again.",
            ));
        }
        let amount_raw = cell(row, amount_col);
        let parsed = decimal_amount(amount_raw)
            .and_then(|a| ferrocast_core::money::Money::parse_decimal_lenient(&a, &currency).ok());
        let Some(money) = parsed else {
            return Err(malformed(
                w,
                format!("row {line}, column \"{}\": \"{amount_raw}\" isn't an amount.", label(amount_col)),
                "Amounts should look like 5, 12.50 or 12,50.",
            ));
        };
        if amount_raw.contains('-') {
            inc.skip(Kind::Tips, format!("Row {line}"), "A refund or negative amount; only tips are imported.");
            continue;
        }
        let name = cell(row, name_col);
        let name = if name.is_empty() { "Anonymous" } else { name };
        let message = message_col.map(|c| cell(row, c)).filter(|m| !m.is_empty());
        let id = id_col
            .map(|c| cell(row, c))
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(64).collect::<String>())
            .unwrap_or_else(|| {
                stable_id(&[&at.to_string(), name, &money.minor.to_string(), &currency, message.unwrap_or("")])
            });
        inc.tips.push(TipRow {
            source_id: id,
            occurred_at: at,
            name: name.chars().take(100).collect(),
            amount_minor: money.minor,
            currency,
            message: message.map(|m| m.chars().take(1_000).collect()),
            provider: "streamlabs".into(),
        });
    }
    Ok(())
}

/// Reads the uploaded files. Errors name the file, row and column.
pub fn parse(files: Files) -> Result<Incoming, EngineError> {
    if files.points.is_none() && files.tips.is_none() {
        return Err(EngineError::Invalid(
            "points: attach your Streamlabs Chatbot points export, a tips export, or both.".into(),
        ));
    }
    if files.points.is_some() && files.platform.is_none() {
        return Err(EngineError::Invalid(
            "platform: say which platform these points are from (twitch, youtube or kick).".into(),
        ));
    }
    let mut inc = Incoming::new(Source::Streamlabs, files.platform);
    if let Some(up) = &files.points {
        inc.kinds.push(Kind::Points);
        points_rows(&mut inc, up)?;
    }
    if let Some(up) = &files.tips {
        inc.kinds.push(Kind::Tips);
        tip_rows(&mut inc, up)?;
    }
    inc.not_imported.push(NotImported {
        what: "Commands and timers".into(),
        count: 0,
        reason: "Streamlabs has no export for Cloudbot commands or timers, and Streamlabs Chatbot's command files use a private format. Add them in Bot; Add starter commands gives you the common ones in one click.".into(),
    });
    inc.not_imported.push(NotImported {
        what: "Alerts, widgets and overlays".into(),
        count: 0,
        reason: "Streamlabs widgets run on Streamlabs' servers and can't be exported. Build them again in Overlays and Alerts.".into(),
    });
    Ok(inc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(name: &str, bytes: &[u8]) -> Upload {
        Upload { file_name: name.into(), bytes: bytes.to_vec() }
    }

    fn status(e: EngineError) -> (u16, String) {
        match e {
            EngineError::Refused { status, message, .. } => (status, message),
            EngineError::Invalid(m) => (400, m),
            other => panic!("unexpected {other:?}"),
        }
    }

    fn points(bytes: &[u8]) -> Result<Incoming, EngineError> {
        parse(Files { platform: Some(Platform::Twitch), points: Some(up("Currency.csv", bytes)), tips: None })
    }

    #[test]
    fn reads_chatbot_currency_exports_in_csv_and_xlsx() {
        let inc = points(
            b"\xEF\xBB\xBFName,Rank,Points,Hours\r\nsam,Viewer,\"1,500\",12\r\n\"ana, the great\",Regular,20.0,1\n\n",
        )
        .unwrap();
        assert_eq!(
            inc.points,
            vec![
                PointsRow { name: "sam".into(), user_id: None, points: 1500 },
                PointsRow { name: "ana, the great".into(), user_id: None, points: 20 },
            ]
        );
        let semi = points(b"Username;Points\nbo;7\n").unwrap();
        assert_eq!(semi.points[0].points, 7);
        let ids = points(b"Name,User ID,Points\nbo,UC123,7\n").unwrap();
        assert_eq!(ids.points[0].user_id.as_deref(), Some("UC123"));
        let wb =
            crate::import::xlsx::tests::workbook(&[&["Name", "Rank", "Points", "Hours"], &["Cy", "Viewer", "42", "3"]]);
        let x = points(&wb).unwrap();
        assert_eq!(x.points, vec![PointsRow { name: "Cy".into(), user_id: None, points: 42 }]);
        let utf16: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("Name\tPoints\nzoë\t5\n".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(points(&utf16).unwrap().points[0].name, "zoë");
        assert!(inc.not_imported.iter().any(|n| n.what == "Commands and timers"), "the report says what can't come");
    }

    #[test]
    fn bad_files_get_the_right_status_and_name_the_row_and_column() {
        let (s, m) = status(points(b"Name,Points\nsam,lots\n").unwrap_err());
        assert_eq!(s, 422);
        assert!(m.contains("row 2") && m.contains("\"Points\"") && m.contains("lots"), "{m}");
        let (s, m) = status(points(b"Name,Score\nsam,1\n").unwrap_err());
        assert_eq!(s, 422);
        assert!(m.contains("no column for points") && m.contains("Found: Name, Score"), "{m}");
        assert_eq!(status(points(b"Name,Points\n,5\n").unwrap_err()).0, 422);
        assert_eq!(status(points(b"Name,Points\nsam,-5\n").unwrap_err()).0, 422);
        assert_eq!(status(points(b"").unwrap_err()).0, 422);
        assert_eq!(status(points(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR").unwrap_err()).0, 415);
        assert_eq!(status(points(b"%PDF-1.7 ...").unwrap_err()).0, 415);
        assert_eq!(status(points(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1]).unwrap_err()).0, 415);
        assert_eq!(status(points(b"PK\x03\x04garbage").unwrap_err()).0, 415);
        assert_eq!(status(points(&vec![b'a'; MAX_FILE + 1]).unwrap_err()).0, 413);
        let (s, m) = status(
            parse(Files { platform: None, points: Some(up("a.csv", b"Name,Points\na,1\n")), tips: None }).unwrap_err(),
        );
        assert_eq!(s, 400);
        assert!(m.starts_with("platform:"));
        assert_eq!(status(parse(Files::default()).unwrap_err()).0, 400);
    }

    #[test]
    fn tip_exports() {
        let csv = "Date,Name,Amount,Currency,Message\n2024-05-01 18:30:00,Big Fan,$12.50,usd,gg\n2024-05-02,Refunder,-3.00,USD,\n";
        let inc =
            parse(Files { platform: None, points: None, tips: Some(up("donations.csv", csv.as_bytes())) }).unwrap();
        assert_eq!(inc.tips.len(), 1);
        let t = &inc.tips[0];
        assert_eq!((t.amount_minor, t.currency.as_str(), t.name.as_str()), (1250, "USD", "Big Fan"));
        assert_eq!(t.occurred_at, 1_714_588_200_000);
        assert_eq!(inc.skipped.len(), 1, "the refund is skipped with a reason");
        let again = parse(Files { platform: None, points: None, tips: Some(up("d.csv", csv.as_bytes())) }).unwrap();
        assert_eq!(again.tips[0].source_id, t.source_id, "the same tip gets the same id every time");
        let bad = "Date,Name,Amount,Currency\n05/01/2024,x,5,USD\n";
        let (s, m) =
            status(parse(Files { platform: None, points: None, tips: Some(up("d.csv", bad.as_bytes())) }).unwrap_err());
        assert_eq!(s, 422);
        assert!(m.starts_with("tips file: row 2, column \"Date\""), "{m}");
        let no_currency = "Date,Name,Amount\n2024-05-01,x,5\n";
        assert_eq!(
            status(
                parse(Files { platform: None, points: None, tips: Some(up("d.csv", no_currency.as_bytes())) })
                    .unwrap_err()
            )
            .0,
            422
        );
    }

    #[test]
    fn csv_quotes_and_separators() {
        assert_eq!(csv("a,\"b \"\"q\"\" c\",d\n"), vec![vec!["a", "b \"q\" c", "d"]]);
        assert_eq!(csv("sep=;\na;b\n"), vec![vec!["a", "b"]]);
        assert_eq!(csv("a,\"multi\nline\"\n"), vec![vec!["a", "multi\nline"]]);
        // "\r\r\n" endings leave blank lines; they never count as rows (or against the cap)
        assert_eq!(csv("a,b\r\r\nc,d\r\r\n\n"), vec![vec!["a", "b"], vec!["c", "d"]]);
        assert_eq!(whole("1 500"), Some(1500));
        for (raw, want) in [
            ("12,50", Some("12.50")),
            ("$1,234.50", Some("1234.50")),
            ("1.234,50 €", Some("1234.50")),
            ("1,234", Some("1234")),
            ("5", Some("5")),
            (",5", Some("0.5")),
            ("12.3456", None),
            ("abc", None),
        ] {
            assert_eq!(decimal_amount(raw).as_deref(), want, "{raw}");
        }
        assert_eq!(whole("12.5"), None);
        assert_eq!(excel_serial("45413"), Some(1_714_521_600_000));
    }
}
