//! Checking the address a streamer types, and normalising a request's `Host`.
//!
//! A custom address is a subdomain the streamer owns (`tips.mychannel.com`): lowercase,
//! internationalised names as punycode, no port, scheme, path, wildcard or IP, never one of
//! Ferrocast's own domains (or a look-alike of one) and never a bare domain
//! (`mychannel.com`), which can't be a CNAME. "Bare" is judged with the whole Public Suffix
//! List built in (`psl`, ICANN and private entries: `co.uk`, `github.io`, `pages.dev`, …).
//! Each label of an internationalised name must be in one script (UTS #39), and no label
//! may be confusable with the name of one of Ferrocast's own domains (`clearcraftlabs`).

use unicode_security::{MixedScript, skeleton};

/// Why an address can't be used. `Apex` carries the registrable domain for the hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostErr {
    Invalid(String),
    Apex(String),
}

/// A checked address: the ASCII name, its Unicode form and the part the streamer registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub hostname: String,
    /// What the streamer reads (`tips.bücher.de` for `tips.xn--bcher-kva.de`).
    pub unicode: String,
    /// `mychannel.com` for `tips.mychannel.com`.
    pub registrable: String,
}

impl Checked {
    /// What goes in the DNS record's name box at most providers (`tips`).
    pub fn label(&self) -> &str {
        self.hostname.strip_suffix(&self.registrable).map_or("", |l| l.trim_end_matches('.'))
    }
}

/// Names that never reach the internet (RFC 6761 and friends).
const SPECIAL_TLDS: &[&str] = &["localhost", "local", "internal", "test", "example", "invalid", "onion", "arpa", "lan"];

fn invalid(m: &str) -> HostErr {
    HostErr::Invalid(m.to_string())
}

/// The registrable part of an ASCII name (`a.b.mychannel.co.uk` → `mychannel.co.uk`,
/// `tips.nova.github.io` → `nova.github.io`), or `None` when the name is itself a suffix.
pub fn registrable(name: &str) -> Option<String> {
    psl::domain_str(name).map(str::to_string)
}

/// The Unicode form of an ASCII (punycode) name, for showing it.
pub fn to_unicode(name: &str) -> String {
    let (u, ok) = idna::domain_to_unicode(name);
    if ok.is_ok() { u } else { name.to_string() }
}

/// The names Ferrocast's own domains are known by (`clearcraftlabs` for
/// `ferrocast-relay.clearcraftlabs.com`): no label of a custom address may look like one.
fn own_names(own: &[String]) -> Vec<String> {
    let mut v: Vec<String> = own
        .iter()
        .filter_map(|o| registrable(o))
        .filter_map(|r| r.split('.').next().map(str::to_string))
        .filter(|l| !l.is_empty())
        .collect();
    v.sort();
    v.dedup();
    v
}

fn skeleton_of(s: &str) -> String {
    skeleton(&s.to_lowercase()).collect::<String>().to_lowercase()
}

/// Checks a typed address. `own` lists Ferrocast's own domains (they and anything under
/// them are refused).
pub fn check(input: &str, own: &[String]) -> Result<Checked, HostErr> {
    let mut s = input.trim();
    for scheme in ["https://", "http://"] {
        if s.get(..scheme.len()).is_some_and(|p| p.eq_ignore_ascii_case(scheme)) {
            s = s.get(scheme.len()..).unwrap_or("");
        }
    }
    s = s.strip_suffix('/').unwrap_or(s);
    if s.is_empty() {
        return Err(invalid("Type the address, like tips.yourchannel.com."));
    }
    if s.contains('*') {
        return Err(invalid("Wildcards (*) aren't supported. Use one address, like tips.yourchannel.com."));
    }
    if s.starts_with('[') || s.parse::<std::net::IpAddr>().is_ok() {
        return Err(invalid("Use a name, not an IP address."));
    }
    if s.contains(['/', '?', '#', '@', ':', '\\']) || s.chars().any(char::is_whitespace) {
        return Err(invalid("Type just the address, like tips.yourchannel.com: no port, path or spaces."));
    }
    let s = s.strip_suffix('.').unwrap_or(s);
    let name = match url::Host::parse(s) {
        Ok(url::Host::Domain(d)) => d,
        Ok(_) => return Err(invalid("Use a name, not an IP address.")),
        Err(_) => return Err(invalid("That isn't a valid address. Use letters, numbers and dashes.")),
    };
    let name = name.to_ascii_lowercase();
    let labels: Vec<&str> = name.split('.').collect();
    let label_ok = |l: &&str| {
        (1..=63).contains(&l.len())
            && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    if name.len() > 253 || labels.len() < 2 || !labels.iter().all(label_ok) {
        return Err(invalid("That isn't a valid address. Use letters, numbers and dashes, like tips.yourchannel.com."));
    }
    let tld = labels[labels.len() - 1];
    if tld.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid("Use a name, not an IP address."));
    }
    if SPECIAL_TLDS.contains(&tld) || name == "home.arpa" {
        return Err(invalid("That address only works on your own network. Use one on the internet."));
    }
    if own.iter().any(|o| name == *o || name.ends_with(&format!(".{o}"))) {
        return Err(invalid("That's one of Ferrocast's own addresses. Use one on a domain you own."));
    }
    let (unicode, decoded) = idna::domain_to_unicode(&name);
    if decoded.is_err() {
        return Err(invalid("That isn't a valid address. Use letters, numbers and dashes, like tips.yourchannel.com."));
    }
    if unicode.split('.').any(|l| !l.is_single_script()) {
        return Err(invalid(
            "Each part of the address has to use one alphabet. Mixing them (like Latin and Cyrillic letters) isn't allowed.",
        ));
    }
    let names: Vec<String> = own_names(own).iter().map(|n| skeleton_of(n)).collect();
    if unicode.split('.').any(|l| names.contains(&skeleton_of(l))) {
        return Err(invalid("That looks like one of Ferrocast's own addresses. Use one on a domain you own."));
    }
    let Some(reg) = registrable(&name) else {
        return Err(invalid("That's a domain ending, not an address. Use one like tips.yourchannel.com."));
    };
    if reg == name {
        return Err(HostErr::Apex(reg));
    }
    Ok(Checked { hostname: name, unicode, registrable: reg })
}

/// A request's `Host`, normalised: lowercase, no port, no trailing dot. IPv6 literals keep
/// their brackets. `None` for an empty or unreadable one.
pub fn normalize_host(raw: &str) -> Option<String> {
    let h = raw.trim().to_ascii_lowercase();
    if h.is_empty() {
        return None;
    }
    if h.starts_with('[') {
        return h.find(']').map(|i| h[..=i].to_string());
    }
    let h = match h.rsplit_once(':') {
        Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) => name.to_string(),
        Some(_) => return None,
        None => h,
    };
    let h = h.strip_suffix('.').unwrap_or(&h).to_string();
    (!h.is_empty()).then_some(h)
}

/// Whether a normalised host can only be Ferrocast's own or a machine's (an IP, a single
/// label such as a container name, `localhost`, or one of `own`).
pub fn is_ours(host: &str, own: &[String]) -> bool {
    host.starts_with('[')
        || host.parse::<std::net::IpAddr>().is_ok()
        || !host.contains('.')
        || host == "localhost"
        || host.ends_with(".localhost")
        || own.iter().any(|o| host == o || host.ends_with(&format!(".{o}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own() -> Vec<String> {
        vec!["clearcraftlabs.com".into(), "tip.example".into()]
    }

    #[test]
    fn addresses_are_normalised() {
        for (typed, want) in [
            ("tips.mychannel.com", "tips.mychannel.com"),
            ("  TIPS.MyChannel.COM.  ", "tips.mychannel.com"),
            ("https://tips.mychannel.com/", "tips.mychannel.com"),
            ("HTTP://tips.mychannel.com", "tips.mychannel.com"),
            ("tips.bücher.de", "tips.xn--bcher-kva.de"),
            ("TIPS.BÜCHER.DE", "tips.xn--bcher-kva.de"),
            ("a.b.mychannel.co.uk", "a.b.mychannel.co.uk"),
            ("tips.my-channel.in", "tips.my-channel.in"),
        ] {
            assert_eq!(check(typed, &own()).map(|c| c.hostname), Ok(want.to_string()), "{typed}");
        }
        let c = check("live.tips.mychannel.co.uk", &own()).unwrap();
        assert_eq!((c.registrable.as_str(), c.label()), ("mychannel.co.uk", "live.tips"));
    }

    #[test]
    fn bad_addresses_are_refused_with_a_reason() {
        for typed in [
            "",
            "   ",
            "*.mychannel.com",
            "tips.*.com",
            "1.2.3.4",
            "[::1]",
            "::1",
            "tips.mychannel.com:8443",
            "tips.mychannel.com/page",
            "tips mychannel.com",
            "user@tips.mychannel.com",
            "localhost",
            "tips.localhost",
            "printer.local",
            "tips.example",
            "tips.clearcraftlabs.com",
            "clearcraftlabs.com",
            "evil.tip.example",
            "-tips.mychannel.com",
            "tips_.mychannel.com",
            "tips..mychannel.com",
            "com",
            "co.uk",
            "1.2.3",
            &format!("{}.mychannel.com", "a".repeat(64)),
        ] {
            assert!(matches!(check(typed, &own()), Err(HostErr::Invalid(_))), "{typed:?} should be invalid");
        }
    }

    #[test]
    fn bare_domains_are_apex() {
        for (typed, reg) in [
            ("mychannel.com", "mychannel.com"),
            ("MyChannel.com.", "mychannel.com"),
            ("mychannel.co.uk", "mychannel.co.uk"),
            ("mychannel.co.in", "mychannel.co.in"),
            ("bücher.de", "xn--bcher-kva.de"),
        ] {
            assert_eq!(check(typed, &own()), Err(HostErr::Apex(reg.into())), "{typed}");
        }
    }

    /// Finding 6: the apex was judged with a short list of two-part endings; now the whole
    /// Public Suffix List, private entries included.
    #[test]
    fn the_public_suffix_list_judges_bare_domains() {
        for (typed, reg) in [
            ("nova.github.io", "nova.github.io"),
            ("nova.pages.dev", "nova.pages.dev"),
            ("nova.vercel.app", "nova.vercel.app"),
            ("mychannel.com.au", "mychannel.com.au"),
            ("mychannel.ac.jp", "mychannel.ac.jp"),
            // `*.kawasaki.jp` is a wildcard rule: one more label is still the bare domain.
            ("nova.mychannel.kawasaki.jp", "nova.mychannel.kawasaki.jp"),
        ] {
            assert_eq!(check(typed, &own()), Err(HostErr::Apex(reg.into())), "{typed}");
        }
        for (typed, reg) in [
            ("tips.nova.github.io", "nova.github.io"),
            ("tips.mychannel.com.au", "mychannel.com.au"),
            ("tips.mychannel.ac.jp", "mychannel.ac.jp"),
        ] {
            assert_eq!(check(typed, &own()).map(|c| c.registrable), Ok(reg.to_string()), "{typed}");
        }
        for typed in ["github.io", "pages.dev", "com.au", "mychannel.kawasaki.jp"] {
            assert!(matches!(check(typed, &own()), Err(HostErr::Invalid(_))), "{typed} is an ending");
        }
    }

    /// Finding 7: a mixed-script label or a look-alike of Ferrocast's own domain is refused,
    /// and the Unicode form is kept for showing.
    #[test]
    fn mixed_scripts_and_look_alikes_are_refused() {
        for typed in [
            "tips.p\u{430}ypal.com",   // Cyrillic а among Latin letters
            "tips.xn--pypal-4ve.com",  // the same, typed as punycode
            "tips.c1earcraftlabs.com", // 1 for l
            "tips.clearcraftlabs.net", // the name itself on another ending
        ] {
            let r = check(typed, &own());
            assert!(matches!(r, Err(HostErr::Invalid(_))), "{typed:?} should be refused: {r:?}");
        }
        for typed in ["tips.ひらがな漢字.jp", "tips.bücher.de", "tips.mychannel.com", "live.clearcraft.com"] {
            assert!(check(typed, &own()).is_ok(), "{typed} is fine");
        }
        let c = check("tips.xn--bcher-kva.de", &own()).unwrap();
        assert_eq!((c.hostname.as_str(), c.unicode.as_str()), ("tips.xn--bcher-kva.de", "tips.bücher.de"));
        assert_eq!(to_unicode("tips.xn--bcher-kva.de"), "tips.bücher.de");
    }

    #[test]
    fn hosts_are_normalised_and_ours_are_recognised() {
        assert_eq!(normalize_host("Tips.MyChannel.com:443").as_deref(), Some("tips.mychannel.com"));
        assert_eq!(normalize_host("tips.mychannel.com.").as_deref(), Some("tips.mychannel.com"));
        assert_eq!(normalize_host("tips.mychannel.com.:8080").as_deref(), Some("tips.mychannel.com"));
        assert_eq!(normalize_host("[::1]:8787").as_deref(), Some("[::1]"));
        assert_eq!(normalize_host("bad:port").as_deref(), None);
        assert_eq!(normalize_host("").as_deref(), None);
        for h in ["127.0.0.1", "[::1]", "ferrocast-relay", "localhost", "tip.example", "x.clearcraftlabs.com"] {
            assert!(is_ours(h, &own()), "{h}");
        }
        assert!(!is_ours("tips.mychannel.com", &own()));
    }
}
