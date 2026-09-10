//! Channel information for the stream-info editor: title, game, tags and language
//! (Modify Channel Information), category search, stream markers and follow dates.
//! Changing the channel needs `channel:manage:broadcast`; older sign-ins don't have it
//! and get a 403 that becomes "reconnect Twitch" (R-22).

use ferrocast_connector::CategoryHit;
use ferrocast_core::stream_info::StreamInfo;
use reqwest::Method;
use serde_json::{Value, json};

use crate::TwitchError;
use crate::helix::Helix;

impl Helix {
    /// Categories matching `query` (Search Categories), up to 10.
    pub async fn search_categories(&self, query: &str) -> Result<Vec<CategoryHit>, TwitchError> {
        let q: String = query.trim().chars().take(100).collect();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let v = self.call(Method::GET, "/search/categories", &[("query", q.as_str()), ("first", "10")], None).await?;
        Ok(v.get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| {
                Some(CategoryHit {
                    id: c.get("id")?.as_str()?.to_string(),
                    name: c.get("name")?.as_str()?.to_string(),
                    image_url: c
                        .get("box_art_url")
                        .and_then(Value::as_str)
                        .map(|u| u.replace("{width}", "52").replace("{height}", "72")),
                })
            })
            .collect())
    }

    /// The category id for a name: an exact (case-blind) match from the search.
    pub async fn category_id(&self, name: &str) -> Result<Option<String>, TwitchError> {
        let hits = self.search_categories(name).await?;
        Ok(hits.into_iter().find(|h| h.name.eq_ignore_ascii_case(name.trim())).map(|h| h.id))
    }

    /// Modify Channel Information. Returns the fields sent.
    pub async fn modify_channel(&self, broadcaster: &str, info: &StreamInfo) -> Result<Vec<String>, TwitchError> {
        let mut body = json!({});
        let mut applied = Vec::new();
        if let Some(t) = &info.title {
            body["title"] = json!(t);
            applied.push("title".to_string());
        }
        if let Some(c) = &info.category {
            let id = match c.ids.twitch.clone() {
                Some(id) => id,
                None => self
                    .category_id(&c.name)
                    .await?
                    .ok_or_else(|| TwitchError::Refused(format!("Twitch has no category called \"{}\"", c.name)))?,
            };
            body["game_id"] = json!(id);
            applied.push("category".to_string());
        }
        if let Some(tags) = &info.tags {
            body["tags"] = json!(tags);
            applied.push("tags".to_string());
        }
        if let Some(l) = &info.language {
            body["broadcaster_language"] = json!(l);
            applied.push("language".to_string());
        }
        self.call(Method::PATCH, "/channels", &[("broadcaster_id", broadcaster)], Some(body)).await?;
        Ok(applied)
    }

    /// Create Stream Marker (only while live; Twitch says so with a 404/400 otherwise).
    pub async fn create_marker(&self, broadcaster: &str, description: &str) -> Result<(), TwitchError> {
        let d: String = description.chars().filter(|c| !c.is_control()).take(140).collect();
        let body = json!({ "user_id": broadcaster, "description": d });
        self.call(Method::POST, "/streams/markers", &[], Some(body)).await.map(|_| ())
    }

    /// When `user_id` followed (Get Channel Followers, `moderator:read:followers`).
    pub async fn followed_at(&self, broadcaster: &str, user_id: &str) -> Result<Option<String>, TwitchError> {
        let q = [("broadcaster_id", broadcaster), ("user_id", user_id)];
        let v = self.call(Method::GET, "/channels/followers", &q, None).await?;
        Ok(v.pointer("/data/0/followed_at").and_then(Value::as_str).map(String::from))
    }
}

/// Unix ms from Twitch's RFC 3339 time (`2024-01-02T03:04:05Z`, fractions allowed).
pub fn rfc3339_ms(s: &str) -> Option<i64> {
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    let time = rest.trim_end_matches('Z');
    let time = time.split(['+']).next()?;
    let mut t = time.split(':');
    let (hh, mm): (i64, i64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?);
    let sec: f64 = t.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    // days from civil (Howard Hinnant)
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400_000 + hh * 3_600_000 + mm * 60_000 + (sec * 1000.0) as i64)
}

#[cfg(test)]
mod tests {
    #[test]
    fn rfc3339() {
        assert_eq!(super::rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(super::rfc3339_ms("2026-10-18T00:00:00Z"), Some(1_792_281_600_000));
        assert_eq!(super::rfc3339_ms("2026-10-18T00:00:01.5Z"), Some(1_792_281_601_500));
        assert_eq!(super::rfc3339_ms("nope"), None);
        assert_eq!(super::rfc3339_ms("2026-13-01T00:00:00Z"), None);
    }
}
