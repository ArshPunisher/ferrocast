//! One stream-info form (title, category, tags, language) for Twitch, YouTube and Kick,
//! with saved presets and a short history. Validation follows the strictest platform, so
//! what passes here is accepted everywhere (or skipped where a platform has no such field).

use serde::{Deserialize, Serialize};

use crate::event::Platform;

pub const MAX_TITLE_CHARS: usize = 140;
pub const MAX_TAGS: usize = 10;
pub const MAX_TAG_CHARS: usize = 25;
pub const MAX_PRESETS: usize = 50;
pub const MAX_HISTORY: usize = 20;
/// YouTube's own limit for a video description.
pub const MAX_DESCRIPTION_CHARS: usize = 5000;

/// A category (Twitch and Kick: a game or "Just Chatting"; YouTube: a video category).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Category {
    pub name: String,
    /// The platform's own id when the streamer picked it from that platform's search.
    #[serde(default)]
    pub ids: CategoryIds,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CategoryIds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub twitch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub youtube: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kick: Option<String>,
}

impl CategoryIds {
    pub fn get(&self, p: Platform) -> Option<&str> {
        match p {
            Platform::Twitch => self.twitch.as_deref(),
            Platform::Youtube => self.youtube.as_deref(),
            Platform::Kick => self.kick.as_deref(),
            _ => None,
        }
    }
}

/// What the streamer wants on every platform. `None` fields are left as they are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StreamInfo {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub category: Option<Category>,
    /// `None` leaves tags alone; `Some([])` clears them.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// ISO 639-1, like `en` or `hi`.
    #[serde(default)]
    pub language: Option<String>,
    /// The stream's description (only YouTube has one). `Some("")` clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// A picture from the media library for the stream's thumbnail (only YouTube takes one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
    /// A platform's own fields, used there instead of the ones above (A2.1). A field left out
    /// of a platform's own uses the everywhere one.
    #[serde(default, skip_serializing_if = "PerPlatform::is_empty")]
    pub per: PerPlatform,
}

/// Per-platform fields: what Twitch, YouTube or Kick gets instead of the everywhere value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PerPlatform {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub twitch: Option<Box<StreamInfo>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub youtube: Option<Box<StreamInfo>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kick: Option<Box<StreamInfo>>,
}

impl PerPlatform {
    pub fn is_empty(&self) -> bool {
        self.twitch.is_none() && self.youtube.is_none() && self.kick.is_none()
    }

    pub fn get(&self, p: Platform) -> Option<&StreamInfo> {
        match p {
            Platform::Twitch => self.twitch.as_deref(),
            Platform::Youtube => self.youtube.as_deref(),
            Platform::Kick => self.kick.as_deref(),
            _ => None,
        }
    }

    fn slots(&mut self) -> [(Platform, &mut Option<Box<StreamInfo>>); 3] {
        [(Platform::Twitch, &mut self.twitch), (Platform::Youtube, &mut self.youtube), (Platform::Kick, &mut self.kick)]
    }
}

impl StreamInfo {
    /// Trims, drops duplicate tags (case-blind) and checks every field. Messages name the field.
    pub fn normalised(mut self) -> Result<StreamInfo, String> {
        let mut per = std::mem::take(&mut self.per);
        for (p, slot) in per.slots() {
            let p = p.label().to_lowercase();
            let Some(own) = slot.take() else { continue };
            if !own.per.is_empty() {
                return Err(format!("per.{p}: a platform's own fields can't have their own per-platform fields."));
            }
            // A platform with nothing of its own just uses the everywhere fields.
            if own.fields().is_empty() {
                continue;
            }
            let own = own.normalised_fields().map_err(|m| format!("per.{p}.{m}"))?;
            *slot = Some(Box::new(own));
        }
        self = self.normalised_fields()?;
        self.per = per;
        if self.fields().is_empty() && self.per.is_empty() {
            return Err("Fill in at least one of title, category, tags, language, description or thumbnail.".into());
        }
        Ok(self)
    }

    /// What `p` gets: the everywhere fields with that platform's own on top.
    pub fn for_platform(&self, p: Platform) -> StreamInfo {
        let mut out = StreamInfo { per: PerPlatform::default(), ..self.clone() };
        if let Some(own) = self.per.get(p) {
            let own = own.clone();
            out.title = own.title.or(out.title);
            out.category = own.category.or(out.category);
            out.tags = own.tags.or(out.tags);
            out.language = own.language.or(out.language);
            out.description = own.description.or(out.description);
            out.thumbnail = own.thumbnail.or(out.thumbnail);
        }
        out
    }

    /// Checks and tidies the fields themselves (not `per`).
    fn normalised_fields(mut self) -> Result<StreamInfo, String> {
        if let Some(t) = &self.title {
            let t: String = t.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
            let t = t.trim().to_string();
            if t.is_empty() {
                return Err("title: write a title, or leave it out to keep the current one.".into());
            }
            if t.chars().count() > MAX_TITLE_CHARS {
                return Err(format!("title: {MAX_TITLE_CHARS} characters at most (Twitch's limit)."));
            }
            self.title = Some(t);
        }
        if let Some(c) = &mut self.category {
            c.name = c.name.trim().to_string();
            if c.name.is_empty() || c.name.chars().count() > 100 {
                return Err("category: pick a category (1 to 100 characters).".into());
            }
            for id in [&c.ids.twitch, &c.ids.youtube, &c.ids.kick].into_iter().flatten() {
                if id.is_empty() || id.len() > 40 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                    return Err("category: that category id isn't one a platform gave.".into());
                }
            }
        }
        if let Some(tags) = &self.tags {
            let mut out: Vec<String> = Vec::new();
            for t in tags {
                let t = t.trim().trim_start_matches('#').to_string();
                if t.is_empty() {
                    continue;
                }
                if t.chars().count() > MAX_TAG_CHARS || !t.chars().all(char::is_alphanumeric) {
                    return Err(format!(
                        "tags: \"{}\" can only have letters and numbers, up to {MAX_TAG_CHARS} (no spaces).",
                        t.chars().take(40).collect::<String>()
                    ));
                }
                if !out.iter().any(|o| o.to_lowercase() == t.to_lowercase()) {
                    out.push(t);
                }
            }
            if out.len() > MAX_TAGS {
                return Err(format!("tags: {MAX_TAGS} tags at most."));
            }
            self.tags = Some(out);
        }
        if let Some(l) = &self.language {
            let l = l.trim().to_lowercase();
            if !(l.len() == 2 && l.bytes().all(|b| b.is_ascii_lowercase())) && l != "other" {
                return Err("language: a two-letter code like en, hi or es.".into());
            }
            self.language = Some(l);
        }
        if let Some(d) = &self.description {
            // Keep line breaks; drop other control characters.
            let d: String = d.chars().filter(|c| !c.is_control() || *c == '\n').collect();
            let d = d.trim().to_string();
            if d.chars().count() > MAX_DESCRIPTION_CHARS {
                return Err(format!("description: {MAX_DESCRIPTION_CHARS} characters at most (YouTube's limit)."));
            }
            if d.contains(['<', '>']) {
                return Err("description: YouTube doesn't allow < or > in a description.".into());
            }
            self.description = Some(d);
        }
        if let Some(t) = &self.thumbnail {
            let t = t.trim().to_string();
            if t.is_empty() || t.len() > 64 || !t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
                return Err("thumbnail: pick a picture from your media library.".into());
            }
            self.thumbnail = Some(t);
        }
        Ok(self)
    }

    /// Fields set, as their API names.
    pub fn fields(&self) -> Vec<&'static str> {
        let mut f = Vec::new();
        if self.title.is_some() {
            f.push("title");
        }
        if self.category.is_some() {
            f.push("category");
        }
        if self.tags.is_some() {
            f.push("tags");
        }
        if self.language.is_some() {
            f.push("language");
        }
        if self.description.is_some() {
            f.push("description");
        }
        if self.thumbnail.is_some() {
            f.push("thumbnail");
        }
        f
    }
}

/// Which fields a platform's API lets apps change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Supports {
    pub title: bool,
    pub category: bool,
    pub tags: bool,
    pub language: bool,
    pub description: bool,
    pub thumbnail: bool,
}

pub fn supports(p: Platform) -> Supports {
    match p {
        // Twitch and Kick have no stream description, and make their own thumbnails from the stream.
        Platform::Twitch => {
            Supports { title: true, category: true, tags: true, language: true, description: false, thumbnail: false }
        }
        // YouTube: the broadcast's video. Its category is a video category ("Gaming"), not a game.
        Platform::Youtube => {
            Supports { title: true, category: true, tags: true, language: true, description: true, thumbnail: true }
        }
        Platform::Kick => {
            Supports { title: true, category: true, tags: true, language: false, description: false, thumbnail: false }
        }
        _ => Supports {
            title: false,
            category: false,
            tags: false,
            language: false,
            description: false,
            thumbnail: false,
        },
    }
}

/// The permission each platform needs to change stream info, and what the dashboard
/// should say when an older sign-in doesn't have it.
pub fn required_scope(p: Platform) -> Option<&'static str> {
    match p {
        Platform::Twitch => Some("channel:manage:broadcast"),
        Platform::Kick => Some("channel:write"),
        Platform::Youtube => Some("https://www.googleapis.com/auth/youtube.force-ssl"),
        _ => None,
    }
}

/// YouTube's video category for a game category (YouTube has no per-game API).
pub fn youtube_category_for(c: &Category) -> &'static str {
    let n = c.name.to_lowercase();
    if n.contains("just chatting") || n.contains("talk") || n.contains("irl") {
        "22" // People & Blogs
    } else if n.contains("music") {
        "10"
    } else if n.contains("art") || n.contains("makers") || n.contains("craft") {
        "26" // Howto & Style
    } else if n.contains("science") || n.contains("technology") || n.contains("software") {
        "28"
    } else {
        "20" // Gaming
    }
}

/// A saved form ("Valorant ranked").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preset {
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub info: StreamInfo,
    #[serde(default)]
    pub updated_at: i64,
}

impl Preset {
    pub fn validate(self, others: &[Preset]) -> Result<Preset, String> {
        let name = self.name.trim().to_string();
        if name.is_empty() || name.chars().count() > 60 {
            return Err("name: give the preset a name of 1 to 60 characters.".into());
        }
        if others.iter().any(|o| o.id != self.id && o.name.eq_ignore_ascii_case(&name)) {
            return Err(format!("A preset called \"{name}\" already exists."));
        }
        let info = self.info.normalised()?;
        Ok(Preset { name, info, ..self })
    }
}

/// One platform's answer when the form was pushed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformResult {
    pub platform: Platform,
    pub ok: bool,
    /// Fields the platform took.
    pub applied: Vec<String>,
    /// Fields it can't take, with why ("Kick has no stream language").
    pub skipped: Vec<String>,
    pub error: Option<PlatformError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformError {
    /// `not_connected`, `reconnect_needed`, `rate_limited`, `refused`, `unreachable`,
    /// `no_broadcast`, `unknown_category`.
    pub code: String,
    pub message: String,
    pub hint: Option<String>,
}

/// A push, kept for "last used".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub info: StreamInfo,
    pub at: i64,
    pub results: Vec<PlatformResult>,
}

/// Saved presets and the last pushes (settings key `stream_info`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Saved {
    pub presets: Vec<Preset>,
    pub history: Vec<HistoryEntry>,
}

impl Saved {
    pub fn remember(&mut self, e: HistoryEntry) {
        self.history.insert(0, e);
        self.history.truncate(MAX_HISTORY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(title: &str) -> StreamInfo {
        StreamInfo { title: Some(title.into()), ..Default::default() }
    }

    #[test]
    fn titles() {
        assert_eq!(info("  Ranked grind\n").normalised().unwrap().title.as_deref(), Some("Ranked grind"));
        assert!(info("   ").normalised().unwrap_err().starts_with("title:"));
        assert!(info(&"x".repeat(141)).normalised().is_err());
        assert!(info(&"é".repeat(140)).normalised().is_ok(), "characters, not bytes");
        assert!(StreamInfo::default().normalised().is_err(), "nothing to change");
    }

    #[test]
    fn tags_and_language() {
        let t =
            StreamInfo { tags: Some(vec!["#English".into(), "english".into(), " ".into(), "FPS".into()]), ..info("x") };
        assert_eq!(t.normalised().unwrap().tags, Some(vec!["English".into(), "FPS".into()]));
        let spaced = StreamInfo { tags: Some(vec!["no spaces".into()]), ..info("x") };
        assert!(spaced.normalised().unwrap_err().starts_with("tags:"));
        let many = StreamInfo { tags: Some((0..11).map(|i| format!("t{i}")).collect()), ..info("x") };
        assert!(many.normalised().is_err());
        let hindi = StreamInfo { tags: Some(vec!["हिंदी".into()]), ..info("x") };
        assert!(hindi.normalised().is_ok(), "Twitch allows tags in any script");
        let lang = StreamInfo { language: Some(" HI ".into()), ..info("x") };
        assert_eq!(lang.normalised().unwrap().language.as_deref(), Some("hi"));
        assert!(StreamInfo { language: Some("eng".into()), ..info("x") }.normalised().is_err());
        let clear = StreamInfo { tags: Some(vec![]), ..Default::default() };
        assert_eq!(clear.normalised().unwrap().fields(), vec!["tags"], "clearing tags is a change");
    }

    #[test]
    fn categories() {
        let c = |name: &str, id: Option<&str>| StreamInfo {
            category: Some(Category {
                name: name.into(),
                ids: CategoryIds { twitch: id.map(String::from), ..Default::default() },
            }),
            ..Default::default()
        };
        assert!(c("Valorant", Some("516575")).normalised().is_ok());
        assert!(c("  ", None).normalised().is_err());
        assert!(c("Valorant", Some("../../x")).normalised().is_err());
        let cat = |n: &str| Category { name: n.into(), ids: CategoryIds::default() };
        assert_eq!(youtube_category_for(&cat("Just Chatting")), "22");
        assert_eq!(youtube_category_for(&cat("Valorant")), "20");
        assert_eq!(youtube_category_for(&cat("Music")), "10");
        assert_eq!(CategoryIds { kick: Some("15".into()), ..Default::default() }.get(Platform::Kick), Some("15"));
    }

    #[test]
    fn presets_and_history() {
        let p = Preset { id: "a".into(), name: " Valorant ranked ".into(), info: info("Ranked"), updated_at: 0 };
        let p = p.validate(&[]).unwrap();
        assert_eq!(p.name, "Valorant ranked");
        let dup = Preset { id: "b".into(), name: "VALORANT RANKED".into(), info: info("x"), updated_at: 0 };
        assert!(dup.validate(std::slice::from_ref(&p)).unwrap_err().contains("already exists"));
        let same_id = Preset { name: "Valorant ranked".into(), ..p.clone() };
        assert!(same_id.validate(std::slice::from_ref(&p)).is_ok(), "renaming itself is fine");
        assert!(Preset { name: "".into(), ..p.clone() }.validate(&[]).is_err());
        let mut s = Saved::default();
        for i in 0..25 {
            s.remember(HistoryEntry { info: info(&i.to_string()), at: i, results: vec![] });
        }
        assert_eq!(s.history.len(), MAX_HISTORY);
        assert_eq!(s.history[0].at, 24, "newest first");
    }

    #[test]
    fn a_platform_can_have_its_own_fields() {
        let json = serde_json::json!({
            "title": "Day 41: Immortal or bust",
            "tags": ["valorant"],
            "per": { "twitch": { "title": " Ranked grind | !socials " }, "kick": {} }
        });
        let i: StreamInfo = serde_json::from_value(json).unwrap();
        let i = i.normalised().unwrap();
        assert!(i.per.kick.is_none(), "an empty platform entry is dropped");
        let tw = i.for_platform(Platform::Twitch);
        assert_eq!(tw.title.as_deref(), Some("Ranked grind | !socials"), "trimmed, and Twitch's own");
        assert_eq!(tw.tags, Some(vec!["valorant".to_string()]), "the rest comes from everywhere");
        assert!(tw.per.is_empty());
        assert_eq!(i.for_platform(Platform::Youtube).title.as_deref(), Some("Day 41: Immortal or bust"));
        // Only one platform's own title: still something to send.
        let only = StreamInfo {
            per: PerPlatform { twitch: Some(Box::new(info("Twitch only"))), ..Default::default() },
            ..Default::default()
        };
        let only = only.normalised().unwrap();
        assert_eq!(only.for_platform(Platform::Kick).fields(), Vec::<&str>::new());
        // A platform's own field is checked like any other, and names where it is.
        let bad = StreamInfo {
            per: PerPlatform { youtube: Some(Box::new(info(&"x".repeat(141)))), ..Default::default() },
            ..info("ok")
        };
        assert!(bad.normalised().unwrap_err().starts_with("per.youtube.title:"));
        let nested = StreamInfo {
            per: PerPlatform {
                twitch: Some(Box::new(StreamInfo {
                    per: PerPlatform { kick: Some(Box::new(info("x"))), ..Default::default() },
                    ..info("y")
                })),
                ..Default::default()
            },
            ..info("ok")
        };
        assert!(nested.normalised().unwrap_err().starts_with("per.twitch:"));
        // Old saved presets without `per` still read, and don't write it back.
        let old: StreamInfo = serde_json::from_str(r#"{"title":"x"}"#).unwrap();
        assert!(!serde_json::to_string(&old).unwrap().contains("per"));
        let unknown = serde_json::from_value::<StreamInfo>(serde_json::json!({ "per": { "tiktok": {} } }));
        assert!(unknown.is_err(), "only Twitch, YouTube and Kick");
    }

    #[test]
    fn platform_support_and_scopes() {
        assert!(!supports(Platform::Kick).language);
        assert!(supports(Platform::Twitch).tags);
        assert!(!supports(Platform::Tips).title);
        assert_eq!(required_scope(Platform::Twitch), Some("channel:manage:broadcast"));
        assert_eq!(required_scope(Platform::Tips), None);
    }
}
