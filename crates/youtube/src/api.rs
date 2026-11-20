//! The REST side: tokens (refreshed once at a time and saved before use), finding the
//! live chat, sending, deleting and banning.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::oauth::{self, AppCredentials, Tokens};
use crate::{Endpoints, YoutubeError, now_ms};

pub(crate) type OnTokens = Arc<dyn Fn(&Tokens) -> Result<(), String> + Send + Sync>;

#[derive(Debug, Clone)]
struct MadeBroadcast {
    id: String,
    stream_id: String,
    at: i64,
}

#[derive(Debug, Default)]
struct MadeSlot {
    broadcast: Option<MadeBroadcast>,
    /// Newly created Ferrocast stream cached briefly while YouTube's `liveStreams.list` catches
    /// up. Always superseded once the server lists the same stream id.
    stream: Option<(Value, i64)>,
}

impl MadeSlot {
    fn fresh_broadcast(&self, now: i64) -> Option<&MadeBroadcast> {
        self.broadcast.as_ref().filter(|m| now - m.at < MADE_FRESH_MS)
    }

    fn merge_fresh_stream(&self, streams: &mut Value, now: i64) {
        let Some((s, _)) = self.stream.as_ref().filter(|(_, at)| now - at < MADE_FRESH_MS) else {
            return;
        };
        let Some(sid) = s.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) else {
            return;
        };
        if let Some(items) = streams.get_mut("items").and_then(Value::as_array_mut) {
            if !items.iter().any(|x| x.get("id").and_then(Value::as_str) == Some(sid)) {
                items.push(s.clone());
            }
        } else {
            *streams = json!({ "items": [s.clone()] });
        }
    }
}

/// Who can watch a broadcast Ferrocast makes. Ordered from the most private to the most public.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Privacy {
    Private,
    Unlisted,
    #[default]
    Public,
}

impl Privacy {
    /// `public`, `unlisted` or `private` (what the engine's setting and YouTube's API both say).
    pub fn parse(s: &str) -> Option<Privacy> {
        match s {
            "public" => Some(Privacy::Public),
            "unlisted" => Some(Privacy::Unlisted),
            "private" => Some(Privacy::Private),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Privacy::Public => "public",
            Privacy::Unlisted => "unlisted",
            Privacy::Private => "private",
        }
    }
}

/// Where the Api reads the streamer's privacy choice each time it makes a broadcast, so a change
/// in the settings applies to the next one without rebuilding the Api.
pub type PrivacySource = Arc<dyn Fn() -> Privacy + Send + Sync>;

pub struct Api {
    privacy: PrivacySource,
    pub(crate) http: reqwest::Client,
    pub(crate) ep: Endpoints,
    app: AppCredentials,
    tokens: Mutex<Tokens>,
    on_tokens: OnTokens,
    /// The last refreshed sign-in couldn't be saved (the Keychain): it is kept for this session and
    /// saved again at the next use.
    unsaved: AtomicBool,
    /// When saving was last tried while `unsaved`: not again for a while (a locked Keychain would
    /// otherwise be asked, under the token lock, at every call).
    last_save_try: StdMutex<Option<std::time::Instant>>,
    /// Ban ids by banned channel, so "unban" can find the ban YouTube created.
    bans: StdMutex<HashMap<String, String>>,
    /// Finding or making the normal broadcast, one at a time (CodeRabbit): two calls at once
    /// would each make one. Holds the one made last (id, stream_id, when), which YouTube's list
    /// can be slow to show.
    made: Mutex<MadeSlot>,
    /// Same serialization and short-term cache for Ferrocast's vertical stream and broadcast.
    made_vertical: Mutex<MadeSlot>,
    /// Stream keys (the secret part) the vertical canvas is set to send to by hand (a custom server
    /// on the vertical canvas): such a stream is never the normal one, whatever its title.
    vertical_keys: StdMutex<HashSet<String>>,
    /// The stream and broadcast lists read for stream info, kept briefly (key: the request): a title
    /// and a thumbnail set one after the other listed everything twice. Cleared by any write.
    reads: StdMutex<HashMap<String, (i64, Value)>>,
    /// Counts writes: a list read before one isn't kept after it.
    writes: AtomicU64,
    /// How long a read is kept (ms; 0: never, as before).
    read_ttl_ms: i64,
}

/// How long to wait before asking the Keychain again to save a refreshed sign-in it refused.
const SAVE_RETRY_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// How long a list of streams or broadcasts is kept for the next lookup.
const READ_TTL_MS: i64 = 60_000;

/// How long a broadcast or stream just made is trusted while YouTube's list doesn't show it yet.
const MADE_FRESH_MS: i64 = 10 * 60_000;

/// Upcoming broadcasts scheduled further ahead than this are never auto-taken (e.g. next week's
/// event); only active broadcasts, ones starting soon, or one the user explicitly picked.
const UPCOMING_AHEAD_MS: i64 = 2 * 3_600_000;

/// Upcoming non-default broadcasts scheduled earlier than this are treated as stale/abandoned.
const UPCOMING_PAST_MS: i64 = 12 * 3_600_000;

/// Upper bound on pages followed when paginating `liveStreams` / `liveBroadcasts` (50/page).
const MAX_LIST_PAGES: usize = 20;

/// Standard YouTube RTMPS ingest server, used as read-only reachability fallback when the channel
/// doesn't yet have a usable RTMP stream (e.g. vertical stream not created yet, or HLS-only key).
pub const DEFAULT_INGEST_URL: &str = "rtmps://a.rtmps.youtube.com/live2";

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
        Api {
            privacy: Arc::new(Privacy::default),
            http,
            ep,
            app,
            tokens: Mutex::new(tokens),
            on_tokens,
            unsaved: AtomicBool::new(false),
            last_save_try: StdMutex::new(None),
            bans: StdMutex::new(HashMap::new()),
            made: Mutex::new(MadeSlot::default()),
            made_vertical: Mutex::new(MadeSlot::default()),
            vertical_keys: StdMutex::new(HashSet::new()),
            reads: StdMutex::new(HashMap::new()),
            writes: AtomicU64::new(0),
            read_ttl_ms: READ_TTL_MS,
        }
    }

    /// How long a list of streams or broadcasts is kept for stream info's lookups (the default is
    /// a minute; zero reads every time).
    #[must_use]
    pub fn with_read_cache(mut self, keep: std::time::Duration) -> Self {
        self.read_ttl_ms = i64::try_from(keep.as_millis()).unwrap_or(i64::MAX);
        self
    }

    /// Makes every broadcast this Api creates with the privacy `source` says at that moment.
    #[must_use]
    pub fn with_privacy(mut self, source: PrivacySource) -> Self {
        self.privacy = source;
        self
    }

    pub async fn access(&self) -> Result<String, YoutubeError> {
        let mut t = self.tokens.lock().await;
        if t.needs_refresh(now_ms()) {
            *t = self.refresh_locked(&t).await?;
        } else if self.unsaved.load(Ordering::Acquire) {
            let mut last = self.last_save_try.lock().unwrap_or_else(|p| p.into_inner());
            if last.is_none_or(|at| at.elapsed() >= SAVE_RETRY_EVERY) {
                *last = Some(std::time::Instant::now());
                self.save(&t);
            }
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
        // Kept even when saving it failed (a Keychain hiccup), as Twitch's and Kick's are: dropping
        // it ended the chat, and a provider that rotates the refresh token would sign out for good.
        self.save(&fresh);
        Ok(fresh)
    }

    fn save(&self, t: &Tokens) {
        match (self.on_tokens)(t) {
            Ok(()) => self.unsaved.store(false, Ordering::Release),
            Err(e) => {
                // Logged once, not at every use.
                if !self.unsaved.swap(true, Ordering::AcqRel) {
                    tracing::error!(error = %e, "couldn't save the new YouTube sign-in; it's kept for this session, saving is retried");
                }
            }
        }
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> Result<Value, YoutubeError> {
        let mut token = self.access().await?;
        if method != Method::GET && (path.starts_with("/liveBroadcasts") || path.starts_with("/liveStreams")) {
            // Making, binding or changing a stream or broadcast: the lists read before it may be
            // wrong now. (A video's title or a thumbnail changes neither list's ids or bindings.)
            self.writes.fetch_add(1, Ordering::AcqRel);
            self.reads.lock().unwrap_or_else(|p| p.into_inner()).clear();
        }
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

    /// Fetches all pages of a YouTube list endpoint (`liveStreams`, `liveBroadcasts`), up to
    /// `max_pages` pages. Stops on missing/empty or repeated `nextPageToken`.
    async fn list_paginated(
        &self,
        path: &str,
        base_query: &[(&str, &str)],
        max_pages: usize,
        fresh: bool,
    ) -> Result<Value, YoutubeError> {
        let key = format!("{path}?{base_query:?}&{max_pages}");
        let now = now_ms();
        if !fresh
            && let Some((at, v)) = self.reads.lock().unwrap_or_else(|p| p.into_inner()).get(&key)
            && now - at < self.read_ttl_ms
        {
            return Ok(v.clone());
        }
        let writes_before = self.writes.load(Ordering::Acquire);
        let mut items: Vec<Value> = Vec::new();
        let mut page_token: Option<String> = None;
        let mut seen_tokens: HashSet<String> = HashSet::new();
        for _ in 0..max_pages.max(1) {
            let mut query: Vec<(&str, &str)> = base_query.to_vec();
            if let Some(tok) = page_token.as_deref() {
                query.push(("pageToken", tok));
            }
            let v = self.call(Method::GET, path, &query, None).await?;
            if let Some(arr) = v.get("items").and_then(Value::as_array) {
                items.extend(arr.iter().cloned());
            }
            let next = v
                .get("nextPageToken")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(String::from);
            match next {
                Some(tok) if seen_tokens.insert(tok.clone()) => page_token = Some(tok),
                _ => break,
            }
        }
        let list = json!({ "items": items });
        // Kept for the next lookup (a fresh read leaves one too), unless a write went by meanwhile.
        if self.read_ttl_ms > 0 && self.writes.load(Ordering::Acquire) == writes_before {
            self.reads.lock().unwrap_or_else(|p| p.into_inner()).insert(key, (now_ms(), list.clone()));
        }
        Ok(list)
    }

    /// The channel's streams (1 unit a page). `fresh`: read now, not what stream info kept.
    async fn list_streams(&self, fresh: bool) -> Result<Value, YoutubeError> {
        self.list_paginated(
            "/liveStreams",
            &[("part", "id,snippet,cdn"), ("mine", "true"), ("maxResults", "50")],
            MAX_LIST_PAGES,
            fresh,
        )
        .await
    }

    async fn list_broadcasts(
        &self,
        part: &str,
        status: &str,
        max_pages: usize,
        fresh: bool,
    ) -> Result<Value, YoutubeError> {
        self.list_paginated(
            "/liveBroadcasts",
            &[("part", part), ("broadcastStatus", status), ("broadcastType", "all"), ("maxResults", "50")],
            max_pages,
            fresh,
        )
        .await
    }

    /// The live chat of the channel's active broadcast, if it's live now (1 unit/page).
    pub async fn active_live_chat(&self) -> Result<Option<String>, YoutubeError> {
        let v = self.list_broadcasts("snippet", "active", MAX_LIST_PAGES, true).await?;
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
    /// The channel's eligible normal broadcast (active first, else an upcoming one within the
    /// short window — never a far-ahead scheduled event — unless `selected_main` names a
    /// user-picked broadcast) and the eligible vertical broadcast. Paginated.
    async fn broadcasts(
        &self,
        vertical_streams: &HashSet<String>,
        selected_main: Option<&str>,
        fresh: bool,
    ) -> Result<(Option<Value>, Option<Value>), YoutubeError> {
        let now = now_ms();
        let active = self.list_broadcasts("id,snippet,status,contentDetails", "active", MAX_LIST_PAGES, fresh).await?;
        let upcoming =
            self.list_broadcasts("id,snippet,status,contentDetails", "upcoming", MAX_LIST_PAGES, fresh).await?;
        let active_items = active.get("items").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
        let upcoming_items = upcoming.get("items").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
        let is_vertical = |b: &Value| {
            b.pointer("/contentDetails/boundStreamId")
                .and_then(Value::as_str)
                .is_some_and(|s| vertical_streams.contains(s))
        };
        let main = match selected_main {
            Some(want) => {
                let found = active_items
                    .iter()
                    .chain(upcoming_items.iter())
                    .find(|b| b.get("id").and_then(Value::as_str) == Some(want))
                    .cloned();
                match found {
                    Some(b) => Some(b),
                    None => {
                        return Err(YoutubeError::Refused(
                            "the selected YouTube broadcast wasn't found; pick another or clear the selection".into(),
                        ));
                    }
                }
            }
            None => active_items
                .iter()
                .find(|b| !is_vertical(b))
                .cloned()
                .or_else(|| pick_best_upcoming(upcoming_items.iter().filter(|b| !is_vertical(b)), now)),
        };
        let vertical = active_items
            .iter()
            .find(|b| is_vertical(b))
            .cloned()
            .or_else(|| pick_best_upcoming(upcoming_items.iter().filter(|b| is_vertical(b)), now));
        Ok((main, vertical))
    }

    /// The stream keys the vertical canvas is configured to send to by hand (replaces the last set).
    pub fn set_vertical_keys(&self, keys: impl IntoIterator<Item = String>) {
        *self.vertical_keys.lock().unwrap_or_else(|p| p.into_inner()) = keys.into_iter().collect();
    }

    fn vertical_keys(&self) -> HashSet<String> {
        self.vertical_keys.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub async fn current_broadcast(&self) -> Result<Option<String>, YoutubeError> {
        let streams = self.list_streams(true).await?;
        let (main, _) = self.broadcasts(&main_excluded(&streams, &self.vertical_keys()), None, true).await?;
        Ok(main.and_then(|b| b.get("id").and_then(Value::as_str).map(String::from)))
    }

    /// The videos stream info goes to: the normal broadcast (made now, bound to a variable RTMP
    /// stream when there's none eligible: streaming needs no broadcast made by hand, owner 30 Oct)
    /// and the vertical one when there is one.
    async fn info_targets(&self) -> Result<Vec<String>, YoutubeError> {
        let mut made = self.made.lock().await;
        let now = now_ms();
        // From what the last lookup kept: the title and then the thumbnail set one after the other
        // are one lookup, not two.
        let mut streams = self.list_streams(false).await?;
        made.merge_fresh_stream(&mut streams, now);
        let vertical = vertical_ids(&streams);
        let (main, vert) = self.broadcasts(&main_excluded(&streams, &self.vertical_keys()), None, false).await?;
        let listed = main
            .as_ref()
            .and_then(|b| b.get("id").and_then(Value::as_str))
            .filter(|id| !id.is_empty())
            .map(String::from);
        let main_id = match listed.or_else(|| made.fresh_broadcast(now).map(|m| m.id.clone())) {
            Some(id) => id,
            None => {
                let candidate = main_stream_id(&streams, &vertical, None, &self.vertical_keys()).ok_or_else(|| {
                    YoutubeError::Refused(
                        "your channel has no stream key yet; turn on live streaming in YouTube Studio first".into(),
                    )
                })?;
                let (stream, _, replaced) = self.ensure_good_main_stream(streams, candidate, &mut made).await?;
                if replaced {
                    tracing::info!("YouTube: binding new broadcast to Ferrocast's own key (variable RTMP)");
                }
                let id = self.make_broadcast("Live now", None, (self.privacy)(), &stream).await?;
                made.broadcast = Some(MadeBroadcast { id: id.clone(), stream_id: stream, at: now_ms() });
                id
            }
        };
        drop(made);
        let mut ids = vec![main_id];
        let vert_id = match vert.and_then(|b| b.get("id").and_then(Value::as_str).map(String::from)) {
            Some(id) => Some(id),
            None => self.made_vertical.lock().await.fresh_broadcast(now_ms()).map(|m| m.id.clone()),
        };
        ids.extend(vert_id);
        Ok(ids)
    }

    /// Binds broadcast `broadcast_id` to `stream_id` (`liveBroadcasts.bind`, 50 units), failing with
    /// a clear one-line reason if YouTube refuses.
    async fn bind_broadcast(&self, broadcast_id: &str, stream_id: &str) -> Result<(), YoutubeError> {
        if broadcast_id.is_empty() || stream_id.is_empty() {
            return Err(YoutubeError::Protocol(
                "couldn't bind the live broadcast: missing broadcast or stream id".into(),
            ));
        }
        self.call(
            Method::POST,
            "/liveBroadcasts/bind",
            &[("id", broadcast_id), ("part", "id"), ("streamId", stream_id)],
            None,
        )
        .await
        .map(drop)
        .map_err(|e| wrap_broadcast_error("couldn't bind the live broadcast", e))
    }

    /// A broadcast that starts and ends with the stream, bound to `stream` → its video id
    /// (liveBroadcasts.insert + bind, 100 units). Fails loudly if either step fails.
    async fn make_broadcast(
        &self,
        title: &str,
        description: Option<&str>,
        privacy: Privacy,
        stream: &str,
    ) -> Result<String, YoutubeError> {
        let mut snippet = json!({ "title": title.chars().take(MAX_TITLE_CHARS).collect::<String>(), "scheduledStartTime": rfc3339_utc(now_ms()) });
        if let Some(d) = description {
            snippet["description"] = json!(d);
        }
        let body = json!({
            "snippet": snippet,
            "status": { "privacyStatus": privacy.as_str(), "selfDeclaredMadeForKids": false },
            "contentDetails": { "enableAutoStart": true, "enableAutoStop": true, "monitorStream": { "enableMonitorStream": false } },
        });
        let b = self
            .call(Method::POST, "/liveBroadcasts", &[("part", "id,snippet,status,contentDetails")], Some(body))
            .await
            .map_err(|e| wrap_broadcast_error("couldn't create a live broadcast", e))?;
        let bid = b.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        if bid.is_empty() {
            return Err(YoutubeError::Protocol("YouTube created a broadcast without an id".into()));
        }
        self.bind_broadcast(&bid, stream).await?;
        Ok(bid)
    }

    /// A broadcast that waits for "Go live" in YouTube Studio (auto-start off) is set to start
    /// with the stream (liveBroadcasts.update, 50 units): the normal live never sits waiting
    /// while the streamer is live everywhere else (owner, 30 Oct).
    async fn start_with_the_stream(&self, b: &Value) -> Result<(), YoutubeError> {
        let life = b.pointer("/status/lifeCycleStatus").and_then(Value::as_str).unwrap_or("");
        let auto = b.pointer("/contentDetails/enableAutoStart").and_then(Value::as_bool).unwrap_or(true);
        if auto || matches!(life, "live" | "liveStarting" | "complete" | "revoked") {
            return Ok(());
        }
        let id = b.get("id").and_then(Value::as_str).unwrap_or_default();
        if id.is_empty() {
            return Ok(());
        }
        let mut cd = b.get("contentDetails").cloned().unwrap_or_else(|| json!({}));
        if let Some(o) = cd.as_object_mut() {
            // Read-only ones YouTube refuses back.
            for k in ["boundStreamId", "boundStreamLastUpdateTimeMs"] {
                o.remove(k);
            }
            o.insert("enableAutoStart".into(), json!(true));
            o.insert("enableAutoStop".into(), json!(true));
            // An update must say how the monitor stream is (YouTube requires it).
            o.entry("monitorStream").or_insert_with(|| json!({ "enableMonitorStream": false }));
        }
        self.call(
            Method::PUT,
            "/liveBroadcasts",
            &[("part", "contentDetails")],
            Some(json!({ "id": id, "contentDetails": cd })),
        )
        .await?;
        tracing::info!("YouTube: the broadcast now starts with the stream");
        Ok(())
    }

    /// The broadcast that went live at `since_ms` or after (give or take 15 minutes): its video
    /// id and when it went live, newest first; ended ones, then the live one (1 unit/page).
    pub async fn last_broadcast(&self, since_ms: i64) -> Result<Option<(String, i64)>, YoutubeError> {
        let mut best: Option<(String, i64)> = None;
        for (status, max_pages) in [("completed", 4), ("active", MAX_LIST_PAGES)] {
            let v = self.list_broadcasts("id,snippet", status, max_pages, true).await?;
            for b in v.get("items").and_then(Value::as_array).into_iter().flatten() {
                let (Some(id), Some(start)) = (
                    b.get("id").and_then(Value::as_str),
                    b.pointer("/snippet/actualStartTime").and_then(Value::as_str).and_then(parse_rfc3339_ms),
                ) else {
                    continue;
                };
                if start >= since_ms - 900_000 && best.as_ref().is_none_or(|(_, s)| start > *s) {
                    best = Some((id.to_string(), start));
                }
            }
        }
        Ok(best)
    }

    /// Puts `chapters` ("0:00 Start" lines) at the end of video `id`'s description, replacing
    /// chapters put there before; the rest of it and the other details are kept
    /// (videos.list + videos.update, 51 units).
    pub async fn set_chapters(&self, id: &str, chapters: &str) -> Result<(), YoutubeError> {
        let v = self.call(Method::GET, "/videos", &[("part", "snippet"), ("id", id)], None).await?;
        let mut snippet = v
            .pointer("/items/0/snippet")
            .cloned()
            .ok_or_else(|| YoutubeError::Protocol("the stream's video wasn't found".into()))?;
        if let Some(o) = snippet.as_object_mut() {
            for k in ["publishedAt", "channelId", "thumbnails", "channelTitle", "liveBroadcastContent", "localized"] {
                o.remove(k);
            }
        }
        let old = snippet.get("description").and_then(Value::as_str).unwrap_or_default();
        let description = with_chapters(old, chapters);
        if description.chars().count() > MAX_DESCRIPTION_CHARS {
            return Err(YoutubeError::Refused(format!(
                "the description would pass YouTube's {MAX_DESCRIPTION_CHARS} characters with the chapters"
            )));
        }
        snippet["description"] = json!(description);
        if snippet.get("categoryId").is_none() {
            snippet["categoryId"] = json!("20");
        }
        self.call(Method::PUT, "/videos", &[("part", "snippet")], Some(json!({ "id": id, "snippet": snippet })))
            .await?;
        Ok(())
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
        let mut applied = Vec::new();
        for id in self.info_targets().await? {
            applied = self.update_video(&id, info).await?;
        }
        Ok(applied)
    }

    /// videos.update of one broadcast's video with `info` → what it changed.
    async fn update_video(
        &self,
        id: &str,
        info: &ferrocast_core::stream_info::StreamInfo,
    ) -> Result<Vec<String>, YoutubeError> {
        let v = self.call(Method::GET, "/videos", &[("part", "snippet"), ("id", id)], None).await?;
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
        for id in self.info_targets().await? {
            self.thumbnail_on(&id, image, mime).await?;
        }
        Ok(())
    }

    /// Copies a thumbnail (from YouTube's own image host) onto another video.
    async fn copy_thumbnail(&self, url: &str, video: &str) -> Result<(), YoutubeError> {
        if !thumbnail_host_ok(url, &self.ep.api) {
            return Err(YoutubeError::Protocol("the thumbnail isn't on YouTube's image host".into()));
        }
        let res = self.http.get(url).send().await.map_err(YoutubeError::network)?;
        if !res.status().is_success() {
            return Err(YoutubeError::Protocol(format!("the thumbnail answered {}", res.status().as_u16())));
        }
        let mime = match res.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()) {
            Some(m) if m.starts_with("image/png") => "image/png",
            _ => "image/jpeg",
        };
        let bytes = res.bytes().await.map_err(YoutubeError::network)?;
        if bytes.len() > MAX_THUMBNAIL_BYTES {
            return Err(YoutubeError::Protocol("the thumbnail is too big".into()));
        }
        self.thumbnail_on(video, &bytes, mime).await
    }

    /// thumbnails.set on one video.
    async fn thumbnail_on(&self, id: &str, image: &[u8], mime: &str) -> Result<(), YoutubeError> {
        let url = upload_base(&self.ep.api) + "/thumbnails/set";
        let mut token = self.access().await?;
        for attempt in 0..2 {
            let res = self
                .http
                .post(&url)
                .query(&[("videoId", id), ("uploadType", "media")])
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
    /// Read-only ingest server lookup for Test connection, Pre-flight and Upload check.
    /// Strictly GET-only: never creates, binds or mutates broadcasts or stream keys on YouTube.
    pub async fn peek_ingest(&self) -> Result<String, YoutubeError> {
        let streams = self.list_streams(false).await?;
        let vertical = vertical_ids(&streams);
        let candidate = main_stream_id(&streams, &vertical, None, &self.vertical_keys()).ok_or_else(|| {
            YoutubeError::Refused(
                "your channel has no stream key yet; turn on live streaming in YouTube Studio first".into(),
            )
        })?;
        // If the channel's existing key is non-RTMP (e.g. HLS), going live will swap it for
        // Ferrocast's RTMP stream on YouTube's standard ingest host; test that host read-only.
        let url = find_stream(&streams, &candidate)
            .and_then(usable)
            .map(|i| i.url)
            .unwrap_or_else(|| DEFAULT_INGEST_URL.to_string());
        Ok(url)
    }

    /// Read-only vertical ingest server lookup (GET only, zero writes on YouTube).
    pub async fn peek_vertical_ingest(&self) -> Result<String, YoutubeError> {
        let streams = self.list_streams(false).await?;
        let url = find_good_vertical_stream(&streams)
            .map(|(_, i)| i.url)
            .or_else(|| {
                streams
                    .get("items")
                    .and_then(Value::as_array)
                    .and_then(|items| items.iter().find_map(usable))
                    .map(|i| i.url)
            })
            .unwrap_or_else(|| DEFAULT_INGEST_URL.to_string());
        Ok(url)
    }

    /// Go live everywhere (write path, only when actually going live): prepares the broadcast and
    /// returns the channel's variable RTMP stream server and key.
    pub async fn ingest(&self) -> Result<ferrocast_connector::Ingest, YoutubeError> {
        self.ingest_for(None).await
    }

    /// Like [`Api::ingest`], optionally targeting a broadcast the user explicitly picked (even if
    /// scheduled further ahead than the automatic window).
    pub async fn ingest_for(
        &self,
        selected_broadcast: Option<&str>,
    ) -> Result<ferrocast_connector::Ingest, YoutubeError> {
        let mut made = self.made.lock().await;
        let now = now_ms();
        // Going live reads the channel as it is now.
        let mut streams = self.list_streams(true).await?;
        made.merge_fresh_stream(&mut streams, now);
        let vertical = vertical_ids(&streams);
        // Ferrocast's own vertical streams (and their broadcasts), and keys the streamer made for
        // vertical video, are never the normal one.
        let (main, _) =
            self.broadcasts(&main_excluded(&streams, &self.vertical_keys()), selected_broadcast, true).await?;
        let fresh_made = if selected_broadcast.is_none() { made.fresh_broadcast(now).cloned() } else { None };
        let live_now = main.as_ref().is_some_and(is_live);

        let current_bound: Option<String> = main
            .as_ref()
            .and_then(|b| b.pointer("/contentDetails/boundStreamId").and_then(Value::as_str))
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| if main.is_none() { fresh_made.as_ref().map(|m| m.stream_id.clone()) } else { None });

        let candidate = main_stream_id(&streams, &vertical, current_bound.as_deref(), &self.vertical_keys())
            .ok_or_else(|| {
                YoutubeError::Refused(
                    "your channel has no stream key yet; turn on live streaming in YouTube Studio first".into(),
                )
            })?;

        // A key with fixed resolution, fixed frame rate or non-RTMP ingestion type caps or breaks
        // the stream (19 Nov: 1440p went out as 1080p). Always swap for Ferrocast's variable RTMP
        // key unless the broadcast is already live on air; `just_made` never skips this check.
        let (stream, streams, replaced) = if live_now {
            if stream_id_needs_replacement(&streams, &candidate, &self.vertical_keys()) {
                tracing::warn!("YouTube: the broadcast is already live on its current key, keeping it");
            }
            (candidate, streams, false)
        } else {
            self.ensure_good_main_stream(streams, candidate, &mut made).await?
        };
        if replaced {
            tracing::info!("YouTube: the stream goes to Ferrocast's own key (variable resolution & fps, RTMP)");
        }

        // Ensure a broadcast exists and is bound to `stream`; fail loudly instead of streaming into
        // nothing.
        match &main {
            Some(b) => {
                let bid = b.get("id").and_then(Value::as_str).unwrap_or_default();
                let already_bound = current_bound.as_deref() == Some(stream.as_str());
                if !already_bound {
                    if live_now {
                        return Err(YoutubeError::Refused(
                            "the live broadcast isn't bound to a usable RTMP stream".into(),
                        ));
                    }
                    self.bind_broadcast(bid, &stream).await?;
                }
                if let Err(err) = self.start_with_the_stream(b).await {
                    tracing::warn!(%err, "YouTube: couldn't set the broadcast to start with the stream");
                }
            }
            None => match fresh_made {
                Some(m) => {
                    // Broadcast made moments ago (list may lag). If its stream was replaced or
                    // differs, rebind it now so the stream and broadcast always match.
                    if m.stream_id != stream {
                        self.bind_broadcast(&m.id, &stream).await?;
                        made.broadcast = Some(MadeBroadcast { id: m.id, stream_id: stream.clone(), at: now_ms() });
                    }
                }
                None => {
                    let id = self.make_broadcast("Live now", None, (self.privacy)(), &stream).await?;
                    tracing::info!("YouTube: made a broadcast for the stream");
                    made.broadcast = Some(MadeBroadcast { id, stream_id: stream.clone(), at: now_ms() });
                }
            },
        }
        drop(made);

        // Return the exact stream bound to the broadcast — never fall back to a different stream.
        let chosen = find_stream(&streams, &stream);
        let note = chosen.map(key_note);
        if let Some(n) = &note {
            tracing::info!(stream_id = %stream, key = %n, "YouTube: the stream goes to this key");
        }
        chosen.and_then(usable).map(|i| ferrocast_connector::Ingest { note, ..i }).ok_or_else(|| {
            YoutubeError::Refused(
                "the broadcast's stream key isn't usable for RTMP; check your stream in YouTube Studio".into(),
            )
        })
    }
}

impl Api {
    /// Unified check+swap used by both `info_targets()` and `ingest_for()`: if `stream_id` has
    /// fixed resolution, fixed frame rate, non-RTMP ingestion type, or missing RTMP details,
    /// switches to Ferrocast's own variable RTMP stream.
    async fn ensure_good_main_stream(
        &self,
        streams: Value,
        stream_id: String,
        slot: &mut MadeSlot,
    ) -> Result<(String, Value, bool), YoutubeError> {
        if !stream_id_needs_replacement(&streams, &stream_id, &self.vertical_keys()) {
            return Ok((stream_id, streams, false));
        }
        let (own_id, list, created) = self.own_stream(&streams).await?;
        if let Some(v) = created {
            slot.stream = Some((v, now_ms()));
        }
        Ok((own_id, list, true))
    }

    /// Ferrocast's own normal stream (variable resolution & fps, RTMP), made once and found again
    /// by its title → its id, the stream list with it in, and the newly created item if inserted
    /// (liveStreams.insert, 50 units, the first time).
    async fn own_stream(&self, streams: &Value) -> Result<(String, Value, Option<Value>), YoutubeError> {
        let vertical_keys = self.vertical_keys();
        let found = streams.get("items").and_then(Value::as_array).and_then(|i| {
            i.iter().find(|s| {
                s.pointer("/snippet/title").and_then(Value::as_str) == Some(MAIN_STREAM_TITLE)
                    && !stream_unfit_for_main(s, &vertical_keys)
            })
        });
        if let Some(id) = found.and_then(|s| s.get("id").and_then(Value::as_str)) {
            return Ok((id.to_string(), streams.clone(), None));
        }
        let body = json!({
            "snippet": { "title": MAIN_STREAM_TITLE, "description": "Ferrocast streams here, at any resolution." },
            "cdn": { "ingestionType": "rtmp", "resolution": "variable", "frameRate": "variable" },
            "contentDetails": { "isReusable": true },
        });
        let created = self
            .call(Method::POST, "/liveStreams", &[("part", "id,snippet,cdn,contentDetails")], Some(body))
            .await
            .map_err(|e| wrap_broadcast_error("couldn't create Ferrocast's stream key", e))?;
        let id = created.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        if id.is_empty() || usable(&created).is_none() {
            return Err(YoutubeError::Refused(
                "YouTube hasn't given Ferrocast's key yet; try again in a minute".into(),
            ));
        }
        tracing::info!("YouTube: made Ferrocast's own stream key (variable resolution & fps, RTMP)");
        let mut list = streams.clone();
        if let Some(items) = list.get_mut("items").and_then(Value::as_array_mut) {
            items.push(created.clone());
        } else {
            list = json!({ "items": [created.clone()] });
        }
        Ok((id, list, Some(created)))
    }
}

impl Api {
    /// Go live everywhere, vertical (YouTube Shorts live): Ferrocast's own upright stream,
    /// made once ("Ferrocast vertical", reusable), and a broadcast bound to it that starts and
    /// ends with the stream, with the title and privacy of the channel's normal live.
    /// Serialized like [`Api::ingest`] so concurrent calls or list lag never make duplicate
    /// broadcasts.
    pub async fn vertical_ingest(&self) -> Result<ferrocast_connector::Ingest, YoutubeError> {
        let mut made = self.made_vertical.lock().await;
        let now = now_ms();
        let mut streams = self.list_streams(true).await?;
        made.merge_fresh_stream(&mut streams, now);
        let mut all_vertical = vertical_ids(&streams);

        let (id, ingest) = match find_good_vertical_stream(&streams) {
            Some((id, ingest)) => (id, ingest),
            None => {
                let body = json!({
                    "snippet": { "title": VERTICAL_STREAM_TITLE, "description": "Ferrocast sends the vertical canvas here." },
                    "cdn": { "ingestionType": "rtmp", "resolution": "variable", "frameRate": "variable" },
                    "contentDetails": { "isReusable": true },
                });
                let created = self
                    .call(Method::POST, "/liveStreams", &[("part", "id,snippet,cdn,contentDetails")], Some(body))
                    .await
                    .map_err(|e| wrap_broadcast_error("couldn't create the vertical stream key", e))?;
                let id = created.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                let Some(ingest) = usable(&created).filter(|_| !id.is_empty()) else {
                    return Err(YoutubeError::Refused(
                        "YouTube hasn't given the vertical stream a key yet; try again in a minute".into(),
                    ));
                };
                tracing::info!("YouTube: made Ferrocast's vertical stream");
                made.stream = Some((created, now_ms()));
                (id, ingest)
            }
        };
        all_vertical.insert(id.clone());

        let (normal, vert) = self.broadcasts(&all_vertical, None, true).await?;
        match vert {
            Some(vb) => {
                let live_now = is_live(&vb);
                let bound = vb.pointer("/contentDetails/boundStreamId").and_then(Value::as_str);
                if bound != Some(id.as_str()) {
                    if live_now {
                        if let Some(old) = bound.and_then(|b| find_stream(&streams, b)).and_then(usable) {
                            tracing::warn!("YouTube: vertical broadcast is already live on its key, keeping it");
                            return Ok(old);
                        }
                        return Err(YoutubeError::Refused(
                            "the live vertical broadcast isn't bound to a usable RTMP stream".into(),
                        ));
                    }
                    let bid = vb.get("id").and_then(Value::as_str).unwrap_or_default();
                    self.bind_broadcast(bid, &id).await?;
                }
                if let Err(err) = self.start_with_the_stream(&vb).await {
                    tracing::warn!(%err, "YouTube: couldn't set the vertical broadcast to start with the stream");
                }
                return Ok(ingest);
            }
            None => {
                if let Some(m) = made.fresh_broadcast(now).cloned() {
                    if m.stream_id != id {
                        self.bind_broadcast(&m.id, &id).await?;
                        made.broadcast = Some(MadeBroadcast { id: m.id, stream_id: id, at: now_ms() });
                    }
                    return Ok(ingest);
                }
            }
        }

        let title: String = normal
            .as_ref()
            .and_then(|b| b.pointer("/snippet/title").and_then(Value::as_str))
            .unwrap_or("Live now")
            .chars()
            .take(MAX_TITLE_CHARS)
            .collect();
        // The vertical broadcast copies the normal live's privacy, but never shows more than the
        // streamer chose: with "private" chosen it stays private beside a public normal live.
        let chosen = (self.privacy)();
        let privacy = normal
            .as_ref()
            .and_then(|b| b.pointer("/status/privacyStatus").and_then(Value::as_str))
            .and_then(Privacy::parse)
            .map_or(chosen, |copied| copied.min(chosen));
        let description = normal.as_ref().and_then(|b| b.pointer("/snippet/description").and_then(Value::as_str));
        let bid = self.make_broadcast(&title, description, privacy, &id).await?;
        made.broadcast = Some(MadeBroadcast { id: bid.clone(), stream_id: id, at: now_ms() });
        drop(made);

        // The normal live's thumbnail too, so Shorts doesn't show YouTube's own picture (owner,
        // 30 Oct). A failure leaves YouTube's; the stream goes on.
        if let Some(url) = normal.as_ref().and_then(best_thumbnail) {
            match self.copy_thumbnail(&url, &bid).await {
                Ok(()) => tracing::info!("YouTube: the vertical broadcast has the live's thumbnail"),
                Err(err) => tracing::warn!(%err, "YouTube: the vertical broadcast's thumbnail wasn't copied"),
            }
        }
        tracing::info!("YouTube: made a broadcast for the vertical stream");
        Ok(ingest)
    }
}

/// The upright stream Ferrocast makes for YouTube Shorts live (found again by this title).
pub const VERTICAL_STREAM_TITLE: &str = "Ferrocast vertical";

/// The normal stream Ferrocast makes when the channel's key is fixed or non-RTMP.
pub const MAIN_STREAM_TITLE: &str = "Ferrocast";

/// Single check for whether a YouTube `liveStream` key must be replaced with Ferrocast's own
/// variable-resolution, variable-fps RTMP key. Used by both `info_targets()` and `ingest()`.
pub(crate) fn stream_needs_replacement(s: &Value) -> bool {
    let fixed_res = s.pointer("/cdn/resolution").and_then(Value::as_str).is_some_and(|r| r != "variable");
    let fixed_fps = s.pointer("/cdn/frameRate").and_then(Value::as_str).is_some_and(|f| f != "variable");
    let not_rtmp = s.pointer("/cdn/ingestionType").and_then(Value::as_str).is_some_and(|t| t != "rtmp");
    fixed_res || fixed_fps || not_rtmp || usable(s).is_none()
}

/// Words in a key's title or description that say it was made for upright video (YouTube Shorts
/// live): such a key never carries the normal, horizontal stream (a streamer's own old "vertical"
/// key is 1080p, so a 1440p stream sent there came out at 1080p, 20 Nov). A false match only means
/// Ferrocast's own key is used instead.
const VERTICAL_WORDS: [&str; 5] = ["vertical", "shorts", "portrait", "9:16", "9x16"];

fn looks_vertical(s: &Value) -> bool {
    ["/snippet/title", "/snippet/description"].iter().filter_map(|p| s.pointer(p).and_then(Value::as_str)).any(|t| {
        let t = t.to_lowercase();
        VERTICAL_WORDS.iter().any(|w| t.contains(w))
    })
}

/// A stream made for vertical video: by its words, or because the vertical canvas is set to send to
/// its key (`vertical_keys`, whatever it is called).
fn is_vertical_key(s: &Value, vertical_keys: &HashSet<String>) -> bool {
    looks_vertical(s)
        || s.pointer("/cdn/ingestionInfo/streamName")
            .and_then(Value::as_str)
            .is_some_and(|k| vertical_keys.contains(k.trim()))
}

/// A key that mustn't carry the normal stream: [`stream_needs_replacement`], or one made for
/// vertical video.
pub(crate) fn stream_unfit_for_main(s: &Value, vertical_keys: &HashSet<String>) -> bool {
    stream_needs_replacement(s) || is_vertical_key(s, vertical_keys)
}

/// The streams a normal live never uses: Ferrocast's own vertical one, and any the streamer made
/// for vertical video.
fn main_excluded(list: &Value, vertical_keys: &HashSet<String>) -> HashSet<String> {
    let mut ids = vertical_ids(list);
    ids.extend(
        list.get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|s| is_vertical_key(s, vertical_keys))
            .filter_map(|s| s.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()).map(String::from)),
    );
    ids
}

/// What is said of the key the stream goes to: its title and resolution ("Ferrocast · variable").
fn key_note(s: &Value) -> String {
    let title = s.pointer("/snippet/title").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty());
    let res = s.pointer("/cdn/resolution").and_then(Value::as_str).unwrap_or("variable");
    format!("{} · {res}", title.unwrap_or("Default key"))
}

fn find_stream<'a>(list: &'a Value, id: &str) -> Option<&'a Value> {
    list.get("items")?.as_array()?.iter().find(|s| s.get("id").and_then(Value::as_str) == Some(id))
}

fn stream_id_needs_replacement(list: &Value, id: &str, vertical_keys: &HashSet<String>) -> bool {
    find_stream(list, id).is_none_or(|s| stream_unfit_for_main(s, vertical_keys))
}

/// Whether an upcoming broadcast is eligible to be taken automatically without explicit user
/// selection: never one scheduled further ahead than [`UPCOMING_AHEAD_MS`] (2 h), and non-default
/// ones scheduled earlier than [`UPCOMING_PAST_MS`] (12 h) ago are ignored as stale.
pub(crate) fn is_upcoming_eligible(b: &Value, now: i64) -> bool {
    let is_default = b.pointer("/snippet/isDefaultBroadcast").and_then(Value::as_bool) == Some(true);
    let scheduled = b.pointer("/snippet/scheduledStartTime").and_then(Value::as_str).and_then(parse_rfc3339_ms);
    match scheduled {
        Some(t) if t > now + UPCOMING_AHEAD_MS => false,
        _ if is_default => true,
        Some(t) => t >= now - UPCOMING_PAST_MS,
        None => true,
    }
}

/// Picks the best eligible upcoming broadcast: explicitly scheduled near-now broadcasts first
/// (closest to `now`), then default/unscheduled ones.
fn pick_best_upcoming<'a>(items: impl Iterator<Item = &'a Value>, now: i64) -> Option<Value> {
    let mut best: Option<(u8, i64, &Value)> = None;
    for b in items {
        if !is_upcoming_eligible(b, now) {
            continue;
        }
        let is_default = b.pointer("/snippet/isDefaultBroadcast").and_then(Value::as_bool) == Some(true);
        let scheduled = b.pointer("/snippet/scheduledStartTime").and_then(Value::as_str).and_then(parse_rfc3339_ms);
        let key = match scheduled {
            Some(t) if !is_default => (0u8, t.abs_diff(now).min(i64::MAX as u64) as i64),
            Some(t) => (1u8, t.abs_diff(now).min(i64::MAX as u64) as i64),
            None => (1u8, 0),
        };
        if best.as_ref().is_none_or(|(bt, bd, _)| key < (*bt, *bd)) {
            best = Some((key.0, key.1, b));
        }
    }
    best.map(|(_, _, b)| b.clone())
}

/// Whether a broadcast is on air (or about to be).
fn is_live(b: &Value) -> bool {
    matches!(b.pointer("/status/lifeCycleStatus").and_then(Value::as_str), Some("live" | "liveStarting"))
}

/// All Ferrocast vertical stream ids in a liveStreams list (so none is ever mistaken for main).
fn vertical_ids(list: &Value) -> HashSet<String> {
    list.get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|s| s.pointer("/snippet/title").and_then(Value::as_str) == Some(VERTICAL_STREAM_TITLE))
        .filter_map(|s| s.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()).map(String::from))
        .collect()
}

/// Finds a Ferrocast vertical stream that does not need replacement.
fn find_good_vertical_stream(list: &Value) -> Option<(String, ferrocast_connector::Ingest)> {
    list.get("items")?.as_array()?.iter().find_map(|s| {
        if s.pointer("/snippet/title").and_then(Value::as_str) != Some(VERTICAL_STREAM_TITLE)
            || stream_needs_replacement(s)
        {
            return None;
        }
        let id = s.get("id").and_then(Value::as_str).filter(|id| !id.is_empty())?;
        Some((id.to_string(), usable(s)?))
    })
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn wrap_broadcast_error(action: &str, err: YoutubeError) -> YoutubeError {
    match err {
        YoutubeError::SignInNeeded | YoutubeError::QuotaExceeded | YoutubeError::RateLimited(_) => err,
        YoutubeError::Network(m) => YoutubeError::Network(one_line(&m)),
        YoutubeError::SignIn(m) => YoutubeError::SignIn(one_line(&m)),
        YoutubeError::Refused(m) | YoutubeError::Protocol(m) => {
            let detail = one_line(&m);
            if detail.is_empty() {
                YoutubeError::Refused(action.into())
            } else {
                YoutubeError::Refused(format!("{action}: {detail}"))
            }
        }
    }
}

/// `2026-10-24T10:30:00Z` (or with `.123` and an offset) as Unix ms; None when it isn't one.
pub(crate) fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d, hh, mm, ss) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Fractions of a second, then Z or ±hh:mm.
    let mut rest = &s[19..];
    let mut ms = 0;
    if let Some(f) = rest.strip_prefix('.') {
        let digits = f.bytes().take_while(u8::is_ascii_digit).count();
        ms = format!("{:0<3}", &f[..digits.min(3)]).parse::<i64>().ok()?;
        rest = &f[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        o if o.len() == 6 && (o.starts_with('+') || o.starts_with('-')) && &o[3..4] == ":" => {
            let v = o[1..3].parse::<i64>().ok()? * 3600 + o[4..6].parse::<i64>().ok()? * 60;
            if o.starts_with('-') { -v } else { v }
        }
        _ => return None,
    };
    // Days from civil (H. Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hh * 3600 + mm * 60 + ss - offset) * 1000 + ms)
}

/// The line chapters start under in a description, and YouTube's description limit.
const CHAPTERS_HEAD: &str = "Chapters:\n0:00 ";
const MAX_DESCRIPTION_CHARS: usize = 5000;

/// `description` with `chapters` at its end, under "Chapters:". Chapters put there before
/// (that line followed by a 0:00 one, and the timed lines after) go; all else is kept.
pub(crate) fn with_chapters(description: &str, chapters: &str) -> String {
    let at =
        description.match_indices(CHAPTERS_HEAD).map(|(i, _)| i).find(|&i| i == 0 || description[..i].ends_with('\n'));
    let timed = |line: &str| {
        line.split_once(' ').is_some_and(|(t, _)| t.contains(':') && t.bytes().all(|b| b.is_ascii_digit() || b == b':'))
    };
    let (before, after) = match at {
        Some(at) => {
            let mut end = at;
            for (i, line) in description[at..].split_inclusive('\n').enumerate() {
                if i > 0 && !timed(line) {
                    break;
                }
                end += line.len();
            }
            (&description[..at], &description[end..])
        }
        None => (description, ""),
    };
    let mut parts: Vec<String> =
        [before.trim(), after.trim()].into_iter().filter(|p| !p.is_empty()).map(String::from).collect();
    parts.push(format!("Chapters:\n{}", chapters.trim_end()));
    parts.join("\n\n")
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

/// Picks the stream to send to from a liveStreams list (used by unit tests).
#[cfg(test)]
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

/// Only YouTube's own image host is fetched from (https, `*.ytimg.com`), or the API's own
/// host (a test server): an address in an answer never sends the app anywhere else.
fn thumbnail_host_ok(url: &str, api: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else { return false };
    let youtube = u.scheme() == "https" && u.host_str().is_some_and(|h| h == "ytimg.com" || h.ends_with(".ytimg.com"));
    let same = reqwest::Url::parse(api)
        .ok()
        .is_some_and(|a| a.scheme() == u.scheme() && a.host_str() == u.host_str() && a.port() == u.port());
    youtube || same
}

/// A broadcast's biggest thumbnail's address (a custom one when it was set).
fn best_thumbnail(b: &Value) -> Option<String> {
    ["maxres", "standard", "high", "medium"]
        .iter()
        .find_map(|k| b.pointer(&format!("/snippet/thumbnails/{k}/url")).and_then(Value::as_str))
        .map(String::from)
}

/// The id of the stream the normal live would use before checking whether it needs replacing:
/// the one bound to its broadcast, else the channel's default, else the first usable RTMP one,
/// else any non-vertical stream (so a channel with only an HLS/broken key still triggers
/// replacement instead of failing as "no stream key"); never Ferrocast's vertical stream.
fn main_stream_id(
    list: &Value,
    vertical: &HashSet<String>,
    bound: Option<&str>,
    vertical_keys: &HashSet<String>,
) -> Option<String> {
    let items: Vec<&Value> = list
        .get("items")?
        .as_array()?
        .iter()
        .filter(|s| s.get("id").and_then(Value::as_str).is_some_and(|id| !id.is_empty() && !vertical.contains(id)))
        .collect();
    let id_is = |s: &Value, id: &str| s.get("id").and_then(Value::as_str) == Some(id);
    let default = |s: &Value| s.pointer("/snippet/isDefaultStream").and_then(Value::as_bool) == Some(true);
    // Beyond the bound one (which is replaced if it's a vertical key), keys made for vertical video
    // aren't picked; with only those left, the first is named so it's replaced, not "no key".
    let others: Vec<&Value> = items.iter().copied().filter(|s| !is_vertical_key(s, vertical_keys)).collect();
    let pick = bound
        .and_then(|b| items.iter().copied().find(|s| id_is(s, b)))
        .or_else(|| others.iter().copied().find(|s| default(s)))
        .or_else(|| others.iter().copied().find(|s| usable(s).is_some()))
        .or_else(|| others.first().copied())
        .or_else(|| items.first().copied())?;
    pick.get("id").and_then(Value::as_str).map(String::from)
}

/// A stream's secure RTMP/RTMPS address and key, if it is an RTMP stream and has both.
fn usable(s: &Value) -> Option<ferrocast_connector::Ingest> {
    if s.pointer("/cdn/ingestionType").and_then(Value::as_str).is_some_and(|t| t != "rtmp") {
        return None;
    }
    let info = s.pointer("/cdn/ingestionInfo")?;
    let key = info.get("streamName").and_then(Value::as_str).map(str::trim).filter(|k| !k.is_empty())?;
    let url = info
        .get("rtmpsIngestionAddress")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|u| u.starts_with("rtmps://"))
        .or_else(|| {
            info.get("ingestionAddress")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|u| u.starts_with("rtmp://") || u.starts_with("rtmps://"))
        })?;
    // YouTube's second ingest for the same key, of the same kind as the one chosen (secure when
    // that is): where the stream goes when the first can't be reached.
    let backup = if url.starts_with("rtmps://") {
        info.get("rtmpsBackupIngestionAddress").and_then(Value::as_str)
    } else {
        info.get("backupIngestionAddress").and_then(Value::as_str)
    }
    .map(str::trim)
    .filter(|b| b.starts_with(if url.starts_with("rtmps://") { "rtmps://" } else { "rtmp://" }))
    .map(|b| b.trim_end_matches('/').to_string());
    Some(ferrocast_connector::Ingest {
        url: url.trim_end_matches('/').to_string(),
        key: key.to_string(),
        backup,
        note: None,
    })
}

#[cfg(test)]
mod host_tests {
    use super::thumbnail_host_ok;

    #[test]
    fn only_youtubes_image_host_is_fetched() {
        let api = "https://www.googleapis.com/youtube/v3";
        assert!(thumbnail_host_ok("https://i.ytimg.com/vi/x/maxresdefault.jpg", api));
        assert!(thumbnail_host_ok("https://i9.ytimg.com/vi/x/hq.jpg?sqp=1", api));
        for bad in [
            "http://i.ytimg.com/vi/x.jpg",
            "https://ytimg.com.evil.example/x.jpg",
            "https://evil.example/i.ytimg.com/x.jpg",
            "file:///etc/passwd",
            "http://127.0.0.1:8080/x",
            "not a url",
        ] {
            assert!(!thumbnail_host_ok(bad, api), "{bad}");
        }
        // A test server's own address.
        assert!(thumbnail_host_ok("http://127.0.0.1:5555/img/b1.jpg", "http://127.0.0.1:5555/api"));
    }
}
