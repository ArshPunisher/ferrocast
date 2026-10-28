//! The REST side: tokens (refreshed once at a time and saved before use), finding the
//! live chat, sending, deleting and banning.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex as StdMutex};

use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::oauth::{self, AppCredentials, Tokens};
use crate::{Endpoints, YoutubeError, now_ms};

type OnTokens = Arc<dyn Fn(&Tokens) -> Result<(), String> + Send + Sync>;

pub struct Api {
    pub(crate) http: reqwest::Client,
    pub(crate) ep: Endpoints,
    app: AppCredentials,
    tokens: Mutex<Tokens>,
    on_tokens: OnTokens,
    /// Ban ids by banned channel, so "unban" can find the ban YouTube created.
    bans: StdMutex<HashMap<String, String>>,
}

impl fmt::Debug for Api {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Api").field("api", &self.ep.api).finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Channel {
    pub id: String,
    pub title: String,
    pub avatar_url: Option<String>,
}

impl Api {
    pub fn new(http: reqwest::Client, ep: Endpoints, app: AppCredentials, tokens: Tokens, on_tokens: OnTokens) -> Self {
        Api { http, ep, app, tokens: Mutex::new(tokens), on_tokens, bans: StdMutex::new(HashMap::new()) }
    }

    pub async fn access(&self) -> Result<String, YoutubeError> {
        let mut t = self.tokens.lock().await;
        if t.needs_refresh(now_ms()) {
            *t = self.refresh_locked(&t).await?;
        }
        Ok(t.access_token.clone())
    }

    pub async fn refresh_after_401(&self, seen: &str) -> Result<String, YoutubeError> {
        let mut t = self.tokens.lock().await;
        if t.access_token != seen {
            return Ok(t.access_token.clone());
        }
        *t = self.refresh_locked(&t).await?;
        Ok(t.access_token.clone())
    }

    async fn refresh_locked(&self, current: &Tokens) -> Result<Tokens, YoutubeError> {
        let fresh = oauth::refresh(&self.http, &self.ep, &self.app, current).await?;
        (self.on_tokens)(&fresh)
            .map_err(|e| YoutubeError::Refused(format!("couldn't save the new YouTube sign-in: {e}")))?;
        Ok(fresh)
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> Result<Value, YoutubeError> {
        let mut token = self.access().await?;
        for attempt in 0..2 {
            let mut req =
                self.http.request(method.clone(), format!("{}{path}", self.ep.api)).query(query).bearer_auth(&token);
            if let Some(b) = &body {
                req = req.json(b);
            }
            let res = req.send().await.map_err(YoutubeError::network)?;
            if res.status().as_u16() == 401 && attempt == 0 {
                token = self.refresh_after_401(&token).await?;
                continue;
            }
            if !res.status().is_success() {
                return Err(YoutubeError::from_response(res).await);
            }
            if res.status().as_u16() == 204 {
                return Ok(Value::Null);
            }
            return res.json().await.map_err(YoutubeError::network);
        }
        Err(YoutubeError::SignInNeeded)
    }

    /// The signed-in channel (1 unit).
    pub async fn me(&self) -> Result<Channel, YoutubeError> {
        let v = self.call(Method::GET, "/channels", &[("part", "snippet"), ("mine", "true")], None).await?;
        let item = v
            .pointer("/items/0")
            .ok_or_else(|| YoutubeError::SignIn("This Google account has no YouTube channel.".into()))?;
        Ok(Channel {
            id: item["id"].as_str().unwrap_or("").to_string(),
            title: item.pointer("/snippet/title").and_then(Value::as_str).unwrap_or("YouTube").to_string(),
            avatar_url: item.pointer("/snippet/thumbnails/default/url").and_then(Value::as_str).map(String::from),
        })
    }

    /// The live chat of the channel's active broadcast, if it's live now (1 unit).
    pub async fn active_live_chat(&self) -> Result<Option<String>, YoutubeError> {
        let q = [("part", "snippet"), ("broadcastStatus", "active"), ("broadcastType", "all"), ("maxResults", "5")];
        let v = self.call(Method::GET, "/liveBroadcasts", &q, None).await?;
        Ok(v.get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find_map(|i| i.pointer("/snippet/liveChatId").and_then(Value::as_str).map(String::from)))
    }

    pub async fn send(&self, live_chat_id: &str, text: &str) -> Result<(), YoutubeError> {
        let body = json!({ "snippet": { "liveChatId": live_chat_id, "type": "textMessageEvent",
                                        "textMessageDetails": { "messageText": text } } });
        self.call(Method::POST, "/liveChat/messages", &[("part", "snippet")], Some(body)).await.map(|_| ())
    }

    pub async fn delete(&self, message_id: &str) -> Result<(), YoutubeError> {
        self.call(Method::DELETE, "/liveChat/messages", &[("id", message_id)], None).await.map(|_| ())
    }

    /// Bans a channel from the live chat; `seconds` makes it a timeout.
    pub async fn ban(&self, live_chat_id: &str, channel_id: &str, seconds: Option<u32>) -> Result<(), YoutubeError> {
        let mut snippet = json!({ "liveChatId": live_chat_id, "type": "permanent", "bannedUserDetails": { "channelId": channel_id } });
        if let Some(s) = seconds {
            snippet["type"] = json!("temporary");
            snippet["banDurationSeconds"] = json!(s.clamp(1, 86_400 * 14));
        }
        let v = self
            .call(Method::POST, "/liveChat/bans", &[("part", "snippet")], Some(json!({ "snippet": snippet })))
            .await?;
        if let Some(id) = v.get("id").and_then(Value::as_str) {
            self.bans.lock().unwrap_or_else(|p| p.into_inner()).insert(channel_id.to_string(), id.to_string());
        }
        Ok(())
    }

    /// YouTube unbans by the id of the ban it created; we only know ids for bans made from
    /// this app during this run.
    pub async fn unban(&self, channel_id: &str) -> Result<bool, YoutubeError> {
        let id = self.bans.lock().unwrap_or_else(|p| p.into_inner()).remove(channel_id);
        match id {
            Some(id) => self.call(Method::DELETE, "/liveChat/bans", &[("id", &id)], None).await.map(|_| true),
            None => Ok(false),
        }
    }
}

/// YouTube's video categories that fit a stream (YouTube has no per-game API).
pub const VIDEO_CATEGORIES: &[(&str, &str)] = &[
    ("20", "Gaming"),
    ("22", "People & Blogs"),
    ("24", "Entertainment"),
    ("10", "Music"),
    ("26", "Howto & Style"),
    ("27", "Education"),
    ("28", "Science & Technology"),
    ("17", "Sports"),
    ("23", "Comedy"),
];

/// YouTube's title limit (Twitch allows 140).
pub const MAX_TITLE_CHARS: usize = 100;

impl Api {
    /// The live broadcast, or the next scheduled one (1 unit each).
    pub async fn current_broadcast(&self) -> Result<Option<String>, YoutubeError> {
        for status in ["active", "upcoming"] {
            let q = [("part", "id"), ("broadcastStatus", status), ("broadcastType", "all"), ("maxResults", "1")];
            let v = self.call(Method::GET, "/liveBroadcasts", &q, None).await?;
            if let Some(id) = v.pointer("/items/0/id").and_then(Value::as_str) {
                return Ok(Some(id.to_string()));
            }
        }
        Ok(None)
    }

    /// The live (or next scheduled) broadcast's video id, or the "no broadcast" refusal.
    async fn broadcast_id(&self) -> Result<String, YoutubeError> {
        self.current_broadcast().await?.ok_or_else(|| {
            YoutubeError::Refused(
                "you don't have a live or scheduled stream on YouTube; start or schedule one in YouTube Studio first"
                    .into(),
            )
        })
    }

    /// Changes the broadcast video's title, category, tags, language and description
    /// (`videos.update`, 50 units). The video's snippet is read first so whatever isn't
    /// being changed (the description, unless given) is kept.
    pub async fn update_broadcast_info(
        &self,
        info: &ferrocast_core::stream_info::StreamInfo,
    ) -> Result<Vec<String>, YoutubeError> {
        if let Some(t) = &info.title
            && t.chars().count() > MAX_TITLE_CHARS
        {
            return Err(YoutubeError::Refused(format!("YouTube titles are {MAX_TITLE_CHARS} characters at most")));
        }
        if info.title.is_none()
            && info.category.is_none()
            && info.tags.is_none()
            && info.language.is_none()
            && info.description.is_none()
        {
            // Only a thumbnail: nothing for videos.update to do.
            return Ok(Vec::new());
        }
        let id = self.broadcast_id().await?;
        let v = self.call(Method::GET, "/videos", &[("part", "snippet"), ("id", id.as_str())], None).await?;
        let mut snippet = v
            .pointer("/items/0/snippet")
            .cloned()
            .ok_or_else(|| YoutubeError::Protocol("the broadcast's video wasn't found".into()))?;
        // Read-only fields YouTube refuses back.
        if let Some(o) = snippet.as_object_mut() {
            for k in ["publishedAt", "channelId", "thumbnails", "channelTitle", "liveBroadcastContent", "localized"] {
                o.remove(k);
            }
        }
        let mut applied = Vec::new();
        if let Some(t) = &info.title {
            snippet["title"] = json!(t);
            applied.push("title".to_string());
        }
        if let Some(c) = &info.category {
            let cat =
                c.ids.youtube.clone().unwrap_or_else(|| ferrocast_core::stream_info::youtube_category_for(c).into());
            snippet["categoryId"] = json!(cat);
            applied.push("category".to_string());
        }
        if let Some(tags) = &info.tags {
            snippet["tags"] = json!(tags);
            applied.push("tags".to_string());
        }
        if let Some(l) = &info.language {
            snippet["defaultLanguage"] = json!(l);
            snippet["defaultAudioLanguage"] = json!(l);
            applied.push("language".to_string());
        }
        if let Some(d) = &info.description {
            snippet["description"] = json!(d);
            applied.push("description".to_string());
        }
        if snippet.get("categoryId").is_none() {
            snippet["categoryId"] = json!("20");
        }
        let body = json!({ "id": id, "snippet": snippet });
        self.call(Method::PUT, "/videos", &[("part", "snippet")], Some(body)).await?;
        Ok(applied)
    }

    /// Sets the broadcast's thumbnail (`thumbnails.set`, 50 units): a JPEG or PNG of up to 50 MB.
    pub async fn set_thumbnail(&self, image: &[u8], mime: &str) -> Result<(), YoutubeError> {
        if !matches!(mime, "image/jpeg" | "image/png") {
            return Err(YoutubeError::Refused("YouTube thumbnails must be a JPEG or PNG picture".into()));
        }
        if image.len() > MAX_THUMBNAIL_BYTES {
            return Err(YoutubeError::Refused("YouTube thumbnails can be 50 MB at most".into()));
        }
        let id = self.broadcast_id().await?;
        let url = upload_base(&self.ep.api) + "/thumbnails/set";
        let mut token = self.access().await?;
        for attempt in 0..2 {
            let res = self
                .http
                .post(&url)
                .query(&[("videoId", id.as_str()), ("uploadType", "media")])
                .bearer_auth(&token)
                .header(reqwest::header::CONTENT_TYPE, mime)
                .body(image.to_vec())
                // The client's 20 s is for small calls; a big picture takes longer on a slow line.
                .timeout(thumbnail_deadline(image.len()))
                .send()
                .await
                .map_err(YoutubeError::network)?;
            if res.status().as_u16() == 401 && attempt == 0 {
                token = self.refresh_after_401(&token).await?;
                continue;
            }
            if res.status().is_success() {
                return Ok(());
            }
            let status = res.status().as_u16();
            return Err(match YoutubeError::from_response(res).await {
                // Custom thumbnails are only for channels that verified a phone number.
                YoutubeError::Refused(_) if status == 403 => YoutubeError::Refused(
                    "custom thumbnails need a verified channel; verify yours at youtube.com/verify".into(),
                ),
                e => e,
            });
        }
        Err(YoutubeError::SignInNeeded)
    }
}

/// YouTube's own limit for a thumbnail.
/// `thumbnails.set` takes up to 50 MB (YouTube's API reference, checked 28 Oct 2026; it was 2 MB).
pub const MAX_THUMBNAIL_BYTES: usize = 50 * 1024 * 1024;

/// How long a thumbnail of `bytes` may take to send: 15 s, plus time for it at a slow upload
/// (256 KB/s, about 2 Mbps), at most 5 minutes. A big picture mustn't fail as "too slow".
pub fn thumbnail_deadline(bytes: usize) -> std::time::Duration {
    let send = bytes as u64 / (256 * 1024);
    std::time::Duration::from_secs((15 + send).min(300))
}

/// Uploads go to `/upload/youtube/v3` next to the API (`{api}/upload` for a test server).
fn upload_base(api: &str) -> String {
    match api.strip_suffix("/youtube/v3") {
        Some(host) => format!("{host}/upload/youtube/v3"),
        None => format!("{api}/upload"),
    }
}

/// The video categories matching `query` (no API call, no quota).
pub fn search_video_categories(query: &str) -> Vec<ferrocast_connector::CategoryHit> {
    let q = query.trim().to_lowercase();
    VIDEO_CATEGORIES
        .iter()
        .filter(|(_, n)| q.is_empty() || n.to_lowercase().contains(&q))
        .map(|(id, n)| ferrocast_connector::CategoryHit { id: (*id).into(), name: (*n).into(), image_url: None })
        .collect()
}

impl Api {
    /// Go live everywhere: the stream key and secure server for this channel, never pasted.
    /// The key bound to the live (or next scheduled) broadcast wins, then the channel's
    /// default key, then the first one (liveBroadcasts + liveStreams, 1 unit each).
    pub async fn ingest(&self) -> Result<ferrocast_connector::Ingest, YoutubeError> {
        let q = [("part", "id,snippet,cdn"), ("mine", "true"), ("maxResults", "50")];
        let mut v = self.call(Method::GET, "/liveStreams", &q, None).await?;
        // Ferrocast's own vertical stream (and its broadcast) is never the normal one.
        let vertical = vertical_id(&v);
        let mut bound = None;
        'status: for status in ["active", "upcoming"] {
            let q = [
                ("part", "contentDetails"),
                ("broadcastStatus", status),
                ("broadcastType", "all"),
                ("maxResults", "5"),
            ];
            let b = self.call(Method::GET, "/liveBroadcasts", &q, None).await?;
            for item in b.get("items").and_then(Value::as_array).into_iter().flatten() {
                let id = item.pointer("/contentDetails/boundStreamId").and_then(Value::as_str);
                if let Some(id) = id.filter(|id| Some(*id) != vertical.as_deref()) {
                    bound = Some(id.to_string());
                    break 'status;
                }
            }
        }
        if let (Some(id), Some(items)) = (&vertical, v.get_mut("items").and_then(Value::as_array_mut)) {
            items.retain(|s| s.get("id").and_then(Value::as_str) != Some(id));
        }
        pick_stream(&v, bound.as_deref()).ok_or_else(|| {
            YoutubeError::Refused(
                "your channel has no stream key yet; turn on live streaming in YouTube Studio first".into(),
            )
        })
    }
}

impl Api {
    /// Go live everywhere, vertical (YouTube Shorts live): Ferrocast's own upright stream,
    /// made once ("Ferrocast vertical", reusable), and a broadcast bound to it that starts and
    /// ends with the stream, with the title and privacy of the channel's normal live.
    /// Costs 2 units when both exist, about 150 the first time (insert stream, broadcast, bind).
    pub async fn vertical_ingest(&self) -> Result<ferrocast_connector::Ingest, YoutubeError> {
        let q = [("part", "id,snippet,cdn"), ("mine", "true"), ("maxResults", "50")];
        let streams = self.call(Method::GET, "/liveStreams", &q, None).await?;
        let (id, ingest) = match vertical_id(&streams) {
            Some(id) => {
                let s = streams
                    .get("items")
                    .and_then(Value::as_array)
                    .and_then(|i| i.iter().find(|s| s.get("id").and_then(Value::as_str) == Some(id.as_str())));
                (id, s.and_then(usable))
            }
            None => {
                let body = json!({
                    "snippet": { "title": VERTICAL_STREAM_TITLE, "description": "Ferrocast sends OBS's vertical canvas here." },
                    "cdn": { "ingestionType": "rtmp", "resolution": "variable", "frameRate": "variable" },
                    "contentDetails": { "isReusable": true },
                });
                let made = self
                    .call(Method::POST, "/liveStreams", &[("part", "id,snippet,cdn,contentDetails")], Some(body))
                    .await?;
                let id = made.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                tracing::info!("YouTube: made Ferrocast's vertical stream");
                (id, usable(&made))
            }
        };
        let ingest = match (id.is_empty(), ingest) {
            (false, Some(i)) => i,
            _ => {
                return Err(YoutubeError::Refused(
                    "YouTube hasn't given the vertical stream a key yet; try again in a minute".into(),
                ));
            }
        };
        // A broadcast already bound to it, or the normal live to copy the title and privacy from.
        let mut normal: Option<Value> = None;
        for status in ["active", "upcoming"] {
            let q = [
                ("part", "id,snippet,status,contentDetails"),
                ("broadcastStatus", status),
                ("broadcastType", "all"),
                ("maxResults", "10"),
            ];
            let v = self.call(Method::GET, "/liveBroadcasts", &q, None).await?;
            for b in v.get("items").and_then(Value::as_array).into_iter().flatten() {
                let bound = b.pointer("/contentDetails/boundStreamId").and_then(Value::as_str);
                if bound == Some(id.as_str()) {
                    return Ok(ingest);
                }
                if normal.is_none() {
                    normal = Some(b.clone());
                }
            }
        }
        let title: String = normal
            .as_ref()
            .and_then(|b| b.pointer("/snippet/title").and_then(Value::as_str))
            .unwrap_or("Live now")
            .chars()
            .take(100)
            .collect();
        let privacy = normal
            .as_ref()
            .and_then(|b| b.pointer("/status/privacyStatus").and_then(Value::as_str))
            .unwrap_or("public")
            .to_string();
        let body = json!({
            "snippet": { "title": title, "scheduledStartTime": rfc3339_utc(now_ms()) },
            "status": { "privacyStatus": privacy, "selfDeclaredMadeForKids": false },
            "contentDetails": { "enableAutoStart": true, "enableAutoStop": true, "monitorStream": { "enableMonitorStream": false } },
        });
        let b = self
            .call(Method::POST, "/liveBroadcasts", &[("part", "id,snippet,status,contentDetails")], Some(body))
            .await?;
        let bid = b.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        self.call(
            Method::POST,
            "/liveBroadcasts/bind",
            &[("id", bid.as_str()), ("part", "id"), ("streamId", id.as_str())],
            None,
        )
        .await?;
        tracing::info!("YouTube: made a broadcast for the vertical stream");
        Ok(ingest)
    }
}

/// The upright stream Ferrocast makes for YouTube Shorts live (found again by this title).
pub const VERTICAL_STREAM_TITLE: &str = "Ferrocast vertical";

/// Ferrocast's vertical stream in a liveStreams list.
fn vertical_id(list: &Value) -> Option<String> {
    list.get("items")?
        .as_array()?
        .iter()
        .find(|s| s.pointer("/snippet/title").and_then(Value::as_str) == Some(VERTICAL_STREAM_TITLE))
        .and_then(|s| s.get("id").and_then(Value::as_str))
        .map(String::from)
}

/// Unix ms as `2026-10-24T10:30:00Z` (UTC; days-to-civil from H. Hinnant's date algorithms).
pub(crate) fn rfc3339_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Picks the stream to send to from a liveStreams list (see [`Api::ingest`]).
pub(crate) fn pick_stream(list: &Value, bound: Option<&str>) -> Option<ferrocast_connector::Ingest> {
    let items: Vec<&Value> = list.get("items")?.as_array()?.iter().filter(|s| usable(s).is_some()).collect();
    let id_is = |s: &Value, id: &str| s.get("id").and_then(Value::as_str) == Some(id);
    let default = |s: &Value| s.pointer("/snippet/isDefaultStream").and_then(Value::as_bool) == Some(true);
    let pick = bound
        .and_then(|b| items.iter().find(|s| id_is(s, b)))
        .or_else(|| items.iter().find(|s| default(s)))
        .or_else(|| items.first())?;
    usable(pick)
}

/// A stream's secure address and key, if it has both.
fn usable(s: &Value) -> Option<ferrocast_connector::Ingest> {
    let info = s.pointer("/cdn/ingestionInfo")?;
    let key = info.get("streamName").and_then(Value::as_str).map(str::trim).filter(|k| !k.is_empty())?;
    let url = info
        .get("rtmpsIngestionAddress")
        .and_then(Value::as_str)
        .filter(|u| u.starts_with("rtmps://"))
        .or_else(|| info.get("ingestionAddress").and_then(Value::as_str))?;
    Some(ferrocast_connector::Ingest { url: url.trim().trim_end_matches('/').to_string(), key: key.to_string() })
}
