//! Creator profile links, overrides, validation, and resolution for overlays ("My links").
//!
//! Keep pure logic here so the engine, server validation, and tests share identical rules.

use serde::{Deserialize, Serialize};

pub const MAX_LINK_CHARS: usize = 120;
pub const MAX_LABEL_CHARS: usize = 40;
pub const MAX_CUSTOM_LINKS: usize = 6;

const WEEKDAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomLink {
    pub label: String,
    pub value: String,
}

/// Editable "My links" configuration persisted under `settings["creator_links"]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CreatorLinks {
    pub twitch: Option<String>,
    pub youtube: Option<String>,
    pub kick: Option<String>,
    pub facebook: Option<String>,
    pub discord: Option<String>,
    pub instagram: Option<String>,
    pub x: Option<String>,
    pub tiktok: Option<String>,
    pub website: Option<String>,
    pub schedule_text: Option<String>,
    pub custom: Vec<CustomLink>,
}

/// Effective links pushed into overlay `snapshot.state.links` and live `state` frames.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedLinks {
    pub twitch: Option<String>,
    pub youtube: Option<String>,
    pub kick: Option<String>,
    pub facebook: Option<String>,
    pub discord: Option<String>,
    pub instagram: Option<String>,
    pub x: Option<String>,
    pub tiktok: Option<String>,
    pub website: Option<String>,
    #[serde(default)]
    pub custom: Vec<CustomLink>,
    #[serde(default)]
    pub socials: Vec<String>,
    pub channel_name: Option<String>,
    pub next_stream: Option<String>,
    pub next_stream_at: Option<i64>,
}

/// Lightweight account slice used by [`resolve_links`].
#[derive(Debug, Clone, Copy)]
pub struct ConnectedAccountInfo<'a> {
    pub platform: &'a str,
    pub display_name: &'a str,
    pub login: &'a str,
    pub user_id: &'a str,
}

fn has_unsafe_scheme(raw: &str) -> bool {
    let compact: String = raw.chars().filter(|c| !c.is_ascii_whitespace()).collect::<String>().to_ascii_lowercase();
    let stripped =
        compact.strip_prefix("https://").or_else(|| compact.strip_prefix("http://")).unwrap_or(compact.as_str());
    for candidate in [compact.as_str(), stripped] {
        if candidate.starts_with("javascript:") || candidate.starts_with("vbscript:") || candidate.starts_with("data:")
        {
            return true;
        }
    }
    if let Some((scheme, _)) = compact.split_once("://") {
        let ok = matches!(scheme, "http" | "https");
        if !ok {
            return true;
        }
    }
    false
}

fn check_field(field: &str, raw: &str, max_chars: usize) -> Result<Option<String>, String> {
    if raw.chars().any(char::is_control) {
        return Err(format!("{field}: control characters are not allowed."));
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().count() > max_chars {
        return Err(format!("{field}: must be at most {max_chars} characters."));
    }
    if has_unsafe_scheme(trimmed) {
        return Err(format!("{field}: javascript: and unsafe link schemes are not allowed."));
    }
    Ok(Some(trimmed.to_string()))
}

fn check_opt(field: &str, raw: Option<String>, max_chars: usize) -> Result<Option<String>, String> {
    match raw {
        Some(s) => check_field(field, &s, max_chars),
        None => Ok(None),
    }
}

impl CreatorLinks {
    /// Validates every field and returns normalized trimmed values with blank strings cleared.
    pub fn validated(self) -> Result<Self, String> {
        if self.custom.len() > MAX_CUSTOM_LINKS {
            return Err(format!("custom: add at most {MAX_CUSTOM_LINKS} custom links."));
        }
        let mut custom = Vec::with_capacity(self.custom.len());
        for (idx, row) in self.custom.into_iter().enumerate() {
            let label = check_field(&format!("custom.{idx}.label"), &row.label, MAX_LABEL_CHARS)?;
            let value = check_field(&format!("custom.{idx}.value"), &row.value, MAX_LINK_CHARS)?;
            match (label, value) {
                (None, None) => continue,
                (None, Some(_)) => return Err(format!("custom.{idx}.label: enter a label.")),
                (Some(_), None) => return Err(format!("custom.{idx}.value: enter a link or handle.")),
                (Some(label), Some(value)) => custom.push(CustomLink { label, value }),
            }
        }
        Ok(CreatorLinks {
            twitch: check_opt("twitch", self.twitch, MAX_LINK_CHARS)?,
            youtube: check_opt("youtube", self.youtube, MAX_LINK_CHARS)?,
            kick: check_opt("kick", self.kick, MAX_LINK_CHARS)?,
            facebook: check_opt("facebook", self.facebook, MAX_LINK_CHARS)?,
            discord: check_opt("discord", self.discord, MAX_LINK_CHARS)?,
            instagram: check_opt("instagram", self.instagram, MAX_LINK_CHARS)?,
            x: check_opt("x", self.x, MAX_LINK_CHARS)?,
            tiktok: check_opt("tiktok", self.tiktok, MAX_LINK_CHARS)?,
            website: check_opt("website", self.website, MAX_LINK_CHARS)?,
            schedule_text: check_opt("schedule_text", self.schedule_text, MAX_LINK_CHARS)?,
            custom,
        })
    }
}

/// Derives a public channel URL from stored account identity fields.
/// Returns `None` when the platform lacks a safe public URL from captured fields.
pub fn account_url(platform: &str, login: &str, user_id: &str) -> Option<String> {
    let login = login.trim();
    let user_id = user_id.trim();
    match platform {
        "twitch" => {
            let handle = login.trim_start_matches('@');
            if handle.is_empty() || handle.chars().any(char::is_whitespace) {
                None
            } else {
                Some(format!("twitch.tv/{}", handle.to_ascii_lowercase()))
            }
        }
        "youtube" => {
            if login.starts_with('@') && login.len() > 1 && !login.chars().any(char::is_whitespace) {
                Some(format!("youtube.com/{login}"))
            } else if !user_id.is_empty() && !user_id.chars().any(char::is_whitespace) {
                Some(format!("youtube.com/channel/{user_id}"))
            } else {
                None
            }
        }
        "kick" => {
            // Relay sign-in only calls Kick `/public/v1/users`, which returns `name` (not channel `slug`).
            let handle = login.trim_start_matches('@');
            if handle.is_empty() || handle.chars().any(char::is_whitespace) {
                None
            } else {
                Some(format!("kick.com/{handle}"))
            }
        }
        // Facebook relay `/me/accounts` only requests id, name, access_token, picture.
        "facebook" => None,
        _ => None,
    }
}

fn strip_url_noise(raw: &str) -> &str {
    let mut s = raw.trim();
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("https://") {
        s = &s["https://".len()..];
    } else if lower.starts_with("http://") {
        s = &s["http://".len()..];
    }
    if s.as_bytes().get(..4).is_some_and(|b| b.eq_ignore_ascii_case(b"www.")) {
        s = &s[4..];
    }
    s.trim_end_matches('/')
}

/// Formats a link override or social value cleanly for overlay display (`twitch.tv/...`, etc.).
pub fn normalize_display_link(kind: &str, raw: &str) -> Option<String> {
    if raw.chars().any(char::is_control) || has_unsafe_scheme(raw) {
        return None;
    }
    let clean = strip_url_noise(raw);
    if clean.is_empty() {
        return None;
    }
    if clean.contains('/') {
        return Some(clean.to_string());
    }
    let formatted = match kind {
        "twitch" => format!("twitch.tv/{}", clean.trim_start_matches('@').to_ascii_lowercase()),
        "youtube" => {
            if clean.starts_with('@') {
                format!("youtube.com/{clean}")
            } else {
                format!("youtube.com/@{clean}")
            }
        }
        "kick" => format!("kick.com/{}", clean.trim_start_matches('@')),
        "facebook" => format!("facebook.com/{}", clean.trim_start_matches('@')),
        "discord" => format!("discord.gg/{}", clean.trim_start_matches('@')),
        "instagram" => format!("instagram.com/{}", clean.trim_start_matches('@')),
        "x" => format!("x.com/{}", clean.trim_start_matches('@')),
        "tiktok" => {
            let handle = clean.trim_start_matches('@');
            format!("tiktok.com/@{handle}")
        }
        _ => clean.to_string(),
    };
    Some(formatted)
}

/// Formats an epoch millisecond timestamp into `"Friday, 8 PM UTC"` as a server-side fallback.
pub fn format_utc_schedule(start_ms: i64) -> String {
    let secs = start_ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let day_secs = secs.rem_euclid(86_400);
    // 1970-01-01 was Thursday (index 4).
    let weekday =
        usize::try_from((days + 4).rem_euclid(7)).ok().and_then(|i| WEEKDAYS.get(i)).copied().unwrap_or("Stream");
    let hour24 = day_secs / 3_600;
    let minute = (day_secs % 3_600) / 60;
    let ampm = if hour24 < 12 { "AM" } else { "PM" };
    let hour12 = match hour24 % 12 {
        0 => 12,
        h => h,
    };
    if minute == 0 {
        format!("{weekday}, {hour12} {ampm} UTC")
    } else {
        format!("{weekday}, {hour12}:{minute:02} {ampm} UTC")
    }
}

/// Resolves effective overlay links from saved overrides, connected accounts, and upcoming schedule.
pub fn resolve_links(
    links: &CreatorLinks,
    accounts: &[ConnectedAccountInfo<'_>],
    next_stream_at: Option<i64>,
) -> ResolvedLinks {
    let platform_value = |platform: &str, override_val: &Option<String>| -> Option<String> {
        if let Some(ov) = override_val.as_deref()
            && let Some(clean) = normalize_display_link(platform, ov)
        {
            return Some(clean);
        }
        accounts.iter().find(|a| a.platform == platform).and_then(|a| account_url(a.platform, a.login, a.user_id))
    };

    let twitch = platform_value("twitch", &links.twitch);
    let youtube = platform_value("youtube", &links.youtube);
    let kick = platform_value("kick", &links.kick);
    let facebook = platform_value("facebook", &links.facebook);
    let discord = links.discord.as_deref().and_then(|s| normalize_display_link("discord", s));
    let instagram = links.instagram.as_deref().and_then(|s| normalize_display_link("instagram", s));
    let x = links.x.as_deref().and_then(|s| normalize_display_link("x", s));
    let tiktok = links.tiktok.as_deref().and_then(|s| normalize_display_link("tiktok", s));
    let website = links.website.as_deref().and_then(|s| normalize_display_link("website", s));

    let mut custom = Vec::new();
    for c in &links.custom {
        if let Some(value) = normalize_display_link("website", &c.value) {
            let label = c.label.trim().to_string();
            if !label.is_empty() {
                custom.push(CustomLink { label, value });
            }
        }
    }

    let mut socials = Vec::new();
    for v in [&twitch, &youtube, &kick, &facebook, &discord, &instagram, &x, &tiktok, &website].into_iter().flatten() {
        socials.push(v.clone());
    }
    for c in &custom {
        socials.push(format!("{}: {}", c.label, c.value));
    }

    let channel_name = ["twitch", "youtube", "kick", "facebook"].into_iter().find_map(|p| {
        accounts.iter().find(|a| a.platform == p).and_then(|a| {
            let name = a.display_name.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
            let login = a.login.trim();
            if !login.is_empty() { Some(login.to_string()) } else { None }
        })
    });

    let next_stream = match next_stream_at {
        Some(ms) => Some(format_utc_schedule(ms)),
        None => links.schedule_text.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from),
    };

    ResolvedLinks {
        twitch,
        youtube,
        kick,
        facebook,
        discord,
        instagram,
        x,
        tiktok,
        website,
        custom,
        socials,
        channel_name,
        next_stream,
        next_stream_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_lengths_control_chars_schemes_and_custom_cap() {
        assert!(CreatorLinks { discord: Some("ok\nline".into()), ..Default::default() }.validated().is_err());
        assert!(CreatorLinks { x: Some("ok\0nul".into()), ..Default::default() }.validated().is_err());
        assert!(
            CreatorLinks { website: Some("  JaVaScRiPt:alert(1)".into()), ..Default::default() }.validated().is_err()
        );
        assert!(
            CreatorLinks { instagram: Some("https://javascript:alert(1)".into()), ..Default::default() }
                .validated()
                .is_err()
        );
        assert!(CreatorLinks { tiktok: Some("ftp://bad.example".into()), ..Default::default() }.validated().is_err());
        assert!(CreatorLinks { website: Some("x".repeat(121)), ..Default::default() }.validated().is_err());
        assert!(
            CreatorLinks {
                custom: vec![CustomLink { label: "x".repeat(41), value: "nova.gg".into() }],
                ..Default::default()
            }
            .validated()
            .is_err()
        );
        assert!(
            CreatorLinks {
                custom: (0..7).map(|i| CustomLink { label: format!("L{i}"), value: "nova.gg".into() }).collect(),
                ..Default::default()
            }
            .validated()
            .is_err()
        );
        assert!(
            CreatorLinks {
                custom: vec![CustomLink { label: "   ".into(), value: "nova.gg".into() }],
                ..Default::default()
            }
            .validated()
            .is_err()
        );

        let clean = CreatorLinks {
            discord: Some("  ".into()),
            schedule_text: Some(" Fridays, 8 PM ".into()),
            custom: vec![
                CustomLink { label: "".into(), value: "".into() },
                CustomLink { label: " Merch ".into(), value: " https://shop.nova.gg/ ".into() },
            ],
            ..Default::default()
        }
        .validated()
        .unwrap();
        assert_eq!(clean.discord, None);
        assert_eq!(clean.schedule_text.as_deref(), Some("Fridays, 8 PM"));
        assert_eq!(clean.custom, vec![CustomLink { label: "Merch".into(), value: "https://shop.nova.gg/".into() }]);
    }

    #[test]
    fn resolves_accounts_overrides_and_schedule_precedence() {
        let accounts = [
            ConnectedAccountInfo { platform: "twitch", display_name: "NovaPlays", login: "NovaGG", user_id: "1" },
            ConnectedAccountInfo {
                platform: "youtube",
                display_name: "Nova Plays",
                login: "Nova Plays",
                user_id: "UC999",
            },
            ConnectedAccountInfo { platform: "facebook", display_name: "Nova Page", login: "Nova Page", user_id: "fb" },
        ];
        let links = CreatorLinks {
            twitch: Some("https://twitch.tv/nova_alt/".into()),
            discord: Some("nova".into()),
            instagram: Some("@novagram".into()),
            tiktok: Some("novatok".into()),
            schedule_text: Some("Fridays, 8 PM".into()),
            ..Default::default()
        };
        let res = resolve_links(&links, &accounts, None);
        assert_eq!(res.twitch.as_deref(), Some("twitch.tv/nova_alt"));
        assert_eq!(res.youtube.as_deref(), Some("youtube.com/channel/UC999"));
        assert_eq!(res.facebook, None);
        assert_eq!(res.discord.as_deref(), Some("discord.gg/nova"));
        assert_eq!(res.instagram.as_deref(), Some("instagram.com/novagram"));
        assert_eq!(res.tiktok.as_deref(), Some("tiktok.com/@novatok"));
        assert_eq!(res.channel_name.as_deref(), Some("NovaPlays"));
        assert_eq!(res.next_stream.as_deref(), Some("Fridays, 8 PM"));
        assert_eq!(
            res.socials,
            vec![
                "twitch.tv/nova_alt",
                "youtube.com/channel/UC999",
                "discord.gg/nova",
                "instagram.com/novagram",
                "tiktok.com/@novatok",
            ]
        );

        // 2027-01-15T20:00:00Z = Friday, 8 PM UTC.
        let scheduled = resolve_links(&links, &accounts, Some(1_800_043_200_000));
        assert_eq!(scheduled.next_stream.as_deref(), Some("Friday, 8 PM UTC"));
        assert_eq!(scheduled.next_stream_at, Some(1_800_043_200_000));

        // Multi-byte UTF-8 characters within the first 4 bytes must never panic during normalization.
        assert_eq!(normalize_display_link("website", "https://café.example/").as_deref(), Some("café.example"));
    }
}
