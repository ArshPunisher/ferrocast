//! Facebook's Graph API for one Page: live videos and their comments.

use std::fmt;

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::FacebookError;

/// A Page the person manages, with the token that acts as that Page.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub id: String,
    pub name: String,
    pub access_token: String,
    #[serde(default)]
    pub avatar_url: Option<String>,
}

impl fmt::Debug for Page {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Page").field("id", &self.id).field("name", &self.name).finish_non_exhaustive()
    }
}

/// A live video made on the Page: where to send the picture, and the key. The key is a secret.
#[derive(Clone, PartialEq, Eq)]
pub struct LiveVideo {
    pub id: String,
    pub server: String,
    pub key: String,
    pub permalink: Option<String>,
}

impl fmt::Debug for LiveVideo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveVideo").field("id", &self.id).field("server", &self.server).finish_non_exhaustive()
    }
}

/// One comment on a live video.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub id: String,
    pub message: String,
    pub from_id: Option<String>,
    pub from_name: String,
    pub avatar_url: Option<String>,
    /// Unix ms.
    pub created_ms: i64,
}

/// The Graph API as one Page. The Page token goes in a header, never in an address.
pub struct Graph {
    http: reqwest::Client,
    base: String,
    page_id: String,
    token: String,
}

impl fmt::Debug for Graph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Graph").field("base", &self.base).field("page_id", &self.page_id).finish_non_exhaustive()
    }
}

/// Splits Facebook's one address (`rtmps://host:443/rtmp/<key>`) into the server and the key.
pub fn split_stream_url(url: &str) -> Option<(String, String)> {
    let (server, key) = url.rsplit_once('/')?;
    (!key.is_empty() && (server.starts_with("rtmp://") || server.starts_with("rtmps://")))
        .then(|| (format!("{server}/"), key.to_string()))
}

impl Graph {
    pub fn new(base: &str, page_id: &str, token: &str) -> Self {
        Graph {
            http: crate::http_client(),
            base: base.trim_end_matches('/').to_string(),
            page_id: page_id.to_string(),
            token: token.to_string(),
        }
    }

    pub fn page_id(&self) -> &str {
        &self.page_id
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> Result<Value, FacebookError> {
        let mut req = self.http.request(method, format!("{}{path}", self.base)).bearer_auth(&self.token).query(query);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await.map_err(FacebookError::from_reqwest)?;
        if !res.status().is_success() {
            return Err(FacebookError::from_response(res).await);
        }
        res.json::<Value>().await.map_err(FacebookError::from_reqwest)
    }

    /// A live video on the Page. `scheduled_start` (Unix seconds) makes it a scheduled one that
    /// waits for its time; none makes it live now (the key is for this video only).
    pub async fn create_live_video(
        &self,
        title: &str,
        description: Option<&str>,
        scheduled_start: Option<i64>,
    ) -> Result<LiveVideo, FacebookError> {
        let mut body = json!({ "title": title.chars().take(255).collect::<String>() });
        if let Some(d) = description {
            body["description"] = json!(d);
        }
        match scheduled_start {
            Some(at) => {
                body["status"] = json!("SCHEDULED_UNPUBLISHED");
                body["planned_start_time"] = json!(at);
            }
            None => body["status"] = json!("LIVE_NOW"),
        }
        let v = self
            .call(
                Method::POST,
                &format!("/{}/live_videos", graph_id(&self.page_id)?),
                &[("fields", "id,secure_stream_url,stream_url,permalink_url")],
                Some(body),
            )
            .await?;
        let id = v.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let url = v
            .get("secure_stream_url")
            .and_then(Value::as_str)
            .or_else(|| v.get("stream_url").and_then(Value::as_str))
            .unwrap_or_default();
        let (server, key) = split_stream_url(url)
            .ok_or_else(|| FacebookError::Protocol("a live video without a stream address".into()))?;
        if id.is_empty() {
            return Err(FacebookError::Protocol("a live video without an id".into()));
        }
        Ok(LiveVideo {
            id,
            server,
            key,
            permalink: v.get("permalink_url").and_then(Value::as_str).and_then(public_link),
        })
    }

    /// The Page's live video that is live now, when there is one.
    pub async fn current_live(&self) -> Result<Option<String>, FacebookError> {
        let v = self
            .call(
                Method::GET,
                &format!("/{}/live_videos", graph_id(&self.page_id)?),
                &[("broadcast_status", "[\"LIVE\"]"), ("fields", "id,status"), ("limit", "1")],
                None,
            )
            .await?;
        Ok(v.pointer("/data/0/id").and_then(Value::as_str).map(String::from))
    }

    /// New comments on a live video, oldest first, after `after` (a paging cursor from before).
    pub async fn comments(
        &self,
        live_id: &str,
        after: Option<&str>,
    ) -> Result<(Vec<Comment>, Option<String>), FacebookError> {
        let mut query = vec![
            ("filter", "toplevel"),
            ("order", "chronological"),
            ("live_filter", "no_filter"),
            ("fields", "id,message,created_time,from{id,name,picture}"),
            ("limit", "100"),
        ];
        if let Some(a) = after {
            query.push(("after", a));
        }
        let v = self.call(Method::GET, &format!("/{}/comments", graph_id(live_id)?), &query, None).await?;
        let list = v
            .get("data")
            .and_then(Value::as_array)
            .map(|rows| rows.iter().filter_map(parse_comment).collect())
            .unwrap_or_default();
        let next = v.pointer("/paging/cursors/after").and_then(Value::as_str).map(String::from);
        Ok((list, next))
    }

    pub async fn post_comment(&self, live_id: &str, text: &str) -> Result<(), FacebookError> {
        self.call(Method::POST, &format!("/{}/comments", graph_id(live_id)?), &[], Some(json!({ "message": text })))
            .await
            .map(|_| ())
    }

    pub async fn delete_comment(&self, comment_id: &str) -> Result<(), FacebookError> {
        self.call(Method::DELETE, &format!("/{}", graph_id(comment_id)?), &[], None).await.map(|_| ())
    }

    /// Changes a live video's title and description.
    pub async fn update_live(
        &self,
        live_id: &str,
        title: Option<&str>,
        description: Option<&str>,
    ) -> Result<Vec<String>, FacebookError> {
        let mut body = serde_json::Map::new();
        let mut applied = Vec::new();
        if let Some(t) = title {
            body.insert("title".into(), json!(t.chars().take(255).collect::<String>()));
            applied.push("title".to_string());
        }
        if let Some(d) = description {
            body.insert("description".into(), json!(d));
            applied.push("description".to_string());
        }
        if body.is_empty() {
            return Ok(applied);
        }
        self.call(Method::POST, &format!("/{}", graph_id(live_id)?), &[], Some(Value::Object(body))).await?;
        Ok(applied)
    }
}

/// A link Graph sends back, as an address the dashboard may open: `https` only. Graph writes the
/// permalink relative to facebook.com (`/page/videos/1`), so a path is completed; anything else
/// (`javascript:`, `data:`, `http:`, `//host`, an address with a password) is dropped.
pub(crate) fn public_link(raw: &str) -> Option<String> {
    let full = if raw.starts_with('/') && !raw.starts_with("//") && !raw.contains('\\') {
        format!("https://www.facebook.com{raw}")
    } else {
        raw.to_string()
    };
    let u = url::Url::parse(&full).ok()?;
    (u.scheme() == "https" && u.host_str().is_some() && u.username().is_empty() && u.password().is_none())
        .then(|| u.to_string())
}

/// A Graph id is letters, digits, `_` and `-` and nothing else: no `/`, `.`, `?`, `#` or `%` that
/// could change which address a call goes to (a comment id can be a Page post or a permission
/// path otherwise, and the call carries the Page's token).
fn graph_id(id: &str) -> Result<&str, FacebookError> {
    if (1..=128).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        Ok(id)
    } else {
        Err(FacebookError::Refused("that isn't a Facebook id".into()))
    }
}

fn parse_comment(v: &Value) -> Option<Comment> {
    let id = v.get("id").and_then(Value::as_str)?.to_string();
    let message = v.get("message").and_then(Value::as_str).unwrap_or_default().to_string();
    let created_ms =
        v.get("created_time").and_then(Value::as_str).and_then(parse_facebook_time).unwrap_or_else(crate::now_ms);
    Some(Comment {
        id,
        message,
        from_id: v.pointer("/from/id").and_then(Value::as_str).map(String::from),
        from_name: v.pointer("/from/name").and_then(Value::as_str).unwrap_or("Someone").to_string(),
        avatar_url: v.pointer("/from/picture/data/url").and_then(Value::as_str).map(String::from),
        created_ms,
    })
}

/// `2026-11-22T20:15:04+0000` (Facebook writes the offset without a colon) as Unix ms.
pub(crate) fn parse_facebook_time(s: &str) -> Option<i64> {
    // Seconds are enough: ordering inside a second is the comment id's job.
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let time = rest.get(..8)?;
    let mut t = time.split(':').map(|p| p.parse::<i64>().ok());
    let (h, mi, sec) = (t.next()??, t.next()??, t.next()??);
    let offset = rest.get(8..).unwrap_or("+0000");
    let sign = if offset.starts_with('-') { -1 } else { 1 };
    let digits: String = offset.chars().filter(char::is_ascii_digit).collect();
    let off_min =
        if digits.len() >= 4 { digits[..2].parse::<i64>().ok()? * 60 + digits[2..4].parse::<i64>().ok()? } else { 0 };
    // Days since the Unix epoch (civil from days, Howard Hinnant).
    let (y2, m2) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + h * 3600 + mi * 60 + sec) - sign * off_min * 60) * 1000)
}
