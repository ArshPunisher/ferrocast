//! Small text helpers the importers share: reply variables, dates, names.

/// Rewrites a StreamElements reply as a Ferrocast template: `${user}` becomes `{user}`,
/// braces that were plain text are escaped, and variables Ferrocast has no equivalent for
/// stay as plain text. Returns the template and the variables it couldn't translate.
pub(crate) fn translate_se(reply: &str) -> (String, Vec<String>) {
    let mut out = String::with_capacity(reply.len() + 8);
    let mut unknown = Vec::new();
    let mut rest = reply;
    while let Some(at) = rest.find("${") {
        escape_into(&mut out, &rest[..at]);
        let after = &rest[at + 2..];
        let Some(end) = after.find('}') else {
            escape_into(&mut out, &rest[at..]);
            rest = "";
            break;
        };
        let inner = after[..end].trim();
        match se_variable(inner) {
            Some(v) => out.push_str(v.as_str()),
            None => {
                escape_into(&mut out, &format!("${{{inner}}}"));
                if !unknown.iter().any(|u: &String| u == inner) {
                    unknown.push(inner.to_string());
                }
            }
        }
        rest = &after[end + 1..];
    }
    escape_into(&mut out, rest);
    (out, unknown)
}

fn escape_into(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '{' => out.push_str("{{"),
            '}' => out.push_str("}}"),
            c => out.push(c),
        }
    }
}

/// The Ferrocast template for a StreamElements variable, when there is one.
fn se_variable(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    let v = match lower.as_str() {
        "user" | "sender" | "user.name" | "source" | "sender.name" => "{user}".to_string(),
        "touser" | "touser.name" | "target" => "{target}".to_string(),
        "1:" | "0:" | "args" => "{args}".to_string(),
        "uptime" => "{uptime|default:\"offline\"}".to_string(),
        n if n.len() == 1 && n.as_bytes()[0].is_ascii_digit() && n != "0" => format!("{{arg{n}}}"),
        _ => return None,
    };
    Some(v)
}

/// Whether Ferrocast accepts this as a command name or alias (with its `!`).
pub(crate) fn usable_command_name(n: &str) -> bool {
    let rest = n.strip_prefix('!').unwrap_or("");
    !rest.is_empty() && rest.chars().count() <= 32 && rest.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

/// `discord`, `!discord` or ` !Discord ` → `!Discord`.
pub(crate) fn command_name(raw: &str) -> String {
    format!("!{}", raw.trim().trim_start_matches('!'))
}

/// Days since 1970-01-01 for a calendar date (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

/// Parses `2024-05-01`, `2024-05-01 12:34`, `2024-05-01T12:34:56.789Z`, with an optional
/// `+05:30` offset, into Unix milliseconds. Times without a zone are taken as UTC.
pub(crate) fn parse_datetime(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let part = s.get(from..from + len)?;
        part.bytes().all(|c| c.is_ascii_digit()).then(|| part.parse().ok()).flatten()
    };
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let (y, mo, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    if !(1970..=2200).contains(&y) || !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) {
        return None;
    }
    let mut ms = days_from_civil(y, mo, d) * 86_400_000;
    let mut rest = &s[10..];
    if rest.is_empty() {
        return Some(ms);
    }
    if !rest.starts_with(['T', 't', ' ']) {
        return None;
    }
    rest = &rest[1..];
    let t = rest.as_bytes();
    if t.len() < 5 || t[2] != b':' {
        return None;
    }
    let hh: i64 = rest.get(0..2)?.parse().ok()?;
    let mm: i64 = rest.get(3..5)?.parse().ok()?;
    let mut idx = 5;
    let mut ss = 0;
    if t.get(5) == Some(&b':') {
        ss = rest.get(6..8)?.parse().ok()?;
        idx = 8;
    }
    if hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    ms += (hh * 3600 + mm * 60 + ss) * 1000;
    let mut tail = &rest[idx..];
    if let Some(frac) = tail.strip_prefix('.') {
        let digits: String = frac.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            return None;
        }
        let first3: String = format!("{digits:0<3}").chars().take(3).collect();
        ms += first3.parse::<i64>().ok()?;
        tail = &frac[digits.len()..];
    }
    match tail {
        "" | "Z" | "z" => Some(ms),
        _ => {
            let sign = match tail.as_bytes()[0] {
                b'+' => -1,
                b'-' => 1,
                _ => return None,
            };
            let off = tail[1..].replace(':', "");
            if off.len() != 4 || !off.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            let oh: i64 = off[..2].parse().ok()?;
            let om: i64 = off[2..].parse().ok()?;
            Some(ms + sign * (oh * 3600 + om * 60) * 1000)
        }
    }
}

/// A stable 64-bit FNV-1a hash, so a tip without an id gets the same id on every import.
pub(crate) fn stable_id(parts: &[&str]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in p.bytes().chain(std::iter::once(0x1f)) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("h{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrocast_core::template::{Template, Vars};

    #[test]
    fn streamelements_variables_become_ferrocast_ones() {
        let (t, unknown) = translate_se("Hi ${user}, welcome {friend}! ${touser} ${1} ${1:} ${random.pick 'a' 'b'}");
        assert_eq!(t, "Hi {user}, welcome {{friend}}! {target} {arg1} {args} ${{random.pick 'a' 'b'}}");
        assert_eq!(unknown, vec!["random.pick 'a' 'b'".to_string()]);
        let tpl = Template::parse(&t).unwrap();
        let mut v = Vars::new();
        v.insert("user".into(), "Sam".into());
        assert!(tpl.render(&v).starts_with("Hi Sam, welcome {friend}! "));
        let (up, _) = translate_se("live for ${uptime}");
        assert!(Template::parse(&up).is_ok());
        let (broken, unknown) = translate_se("cost ${ 5 and {x");
        assert_eq!(broken, "cost ${{ 5 and {{x");
        assert!(unknown.is_empty());
        assert!(Template::parse(&broken).is_ok());
    }

    #[test]
    fn dates_parse_strictly() {
        assert_eq!(parse_datetime("1970-01-01"), Some(0));
        assert_eq!(parse_datetime("2018-12-07T17:19:01.957Z"), Some(1_544_203_141_957));
        assert_eq!(parse_datetime("2018-12-07 17:19:01"), Some(1_544_203_141_000));
        assert_eq!(parse_datetime("2018-12-07T22:49:01+05:30"), Some(1_544_203_141_000));
        assert_eq!(parse_datetime("2024-02-29"), Some(1_709_164_800_000));
        for bad in ["2023-02-29", "07/12/2018", "2018-13-01", "2018-12-07T25:00", "2018-12-07Tnope", "", "2018-12-07 1"]
        {
            assert_eq!(parse_datetime(bad), None, "{bad}");
        }
    }

    #[test]
    fn names_and_ids() {
        assert_eq!(command_name(" !Discord "), "!Discord");
        assert!(usable_command_name("!so-2_x"));
        assert!(!usable_command_name("!two words") && !usable_command_name("!") && !usable_command_name("x"));
        assert_eq!(stable_id(&["a", "b"]), stable_id(&["a", "b"]));
        assert_ne!(stable_id(&["ab", ""]), stable_id(&["a", "b"]));
    }
}
