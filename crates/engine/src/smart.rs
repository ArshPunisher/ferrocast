//! Pro smart features in the app (1.4; API.md §4 "Smart features", relay §9.10): smart
//! moderation, smart replies and the written stream recap. The relay asks a language model;
//! everything that happens in chat is decided here.
//!
//! - **Smart moderation** looks only at messages that pass the streamer's own filters and
//!   that a cheap local check finds worth a second look (a first message, a link, a phone
//!   number, words that are often abuse in English, Hindi or Hinglish, shouting, floods).
//!   They're checked in small batches. A flagged message becomes a **suggestion** in the
//!   Moderation page (delete, time out, ban, or dismiss). With "Act on clear cases" on, a
//!   very clear case is **timed out** on its own (never banned, never deleted), logged with
//!   one-click Undo. Suggestions live in memory only (the last [`LOG_KEEP`]).
//! - **Smart replies** answer chat questions from the streamer's own facts, only after the
//!   streamer approved that exact text, only while live, at most one every `reply_gap_s` per
//!   platform and one per viewer every [`VIEWER_GAP_MS`], and never for a command or a
//!   message an auto-reply already answered.
//! - **Stream recap**: a short written recap from the recap's numbers, kept for the last stream.
//!
//! Without Pro, or when the relay has no model set up (`features.smart`), nothing is sent
//! and every switch says why. When the relay says the day's allowance is used up (429 /
//! 503), the feature pauses until it says to try again; the app's own filters, commands and
//! auto-replies never wait on any of this (R-01).

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use ferrocast_core::event::{Event, EventKind, Origin, Platform, Roles};
use ferrocast_store::repo;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::frames::LiveFrame;
use crate::{Engine, EngineError, now, platform_key};

const SETTINGS_KEY: &str = "smart";
const RECAP_KEY: &str = "smart_recap";
/// Suggestions kept in memory for the Moderation page.
pub const LOG_KEEP: usize = 200;
/// Smart replies kept in memory for the Bot page.
pub const REPLIES_KEEP: usize = 50;
/// Messages waiting for a check at most (more are left unchecked rather than piling up).
const QUEUE_MAX: usize = 60;
/// A batch goes when it has this many messages, or its oldest waited [`BATCH_WAIT_MS`].
const BATCH: usize = 20;
const BATCH_WAIT_MS: i64 = 5_000;
/// How often the task looks at the queue (and re-reads Pro from memory).
const TICK: Duration = Duration::from_secs(2);
/// Below this the relay's verdict is ignored; at or above [`ACT_CONFIDENCE`] it may act.
pub const SUGGEST_CONFIDENCE: f64 = 0.6;
pub const ACT_CONFIDENCE: f64 = 0.85;
/// One smart reply per viewer this often.
pub const VIEWER_GAP_MS: i64 = 10 * 60_000;
pub const MAX_RULES: usize = 500;
pub const MAX_ABOUT: usize = 2_000;

/// What smart moderation does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModMode {
    #[default]
    Off,
    /// Suggestions only: a person decides.
    Suggest,
    /// Suggestions, and a very clear case is timed out on its own (with Undo).
    Act,
}

/// Saved in the local store (settings `smart`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SmartSettings {
    pub moderation: ModMode,
    /// How long an automatic timeout lasts (10 s to 10 minutes).
    pub act_timeout_s: u32,
    /// The streamer's own extra words for moderation ("no spoilers").
    pub rules: String,
    pub replies: bool,
    /// The streamer's facts for replies (schedule, rig, links).
    pub about: String,
    /// SHA-256 of the `about` text the streamer approved (replies use only that text).
    pub about_approved: Option<String>,
    /// At most one smart reply per platform this often (30 s to an hour).
    pub reply_gap_s: u32,
}

impl Default for SmartSettings {
    fn default() -> Self {
        SmartSettings {
            moderation: ModMode::Off,
            act_timeout_s: 60,
            rules: String::new(),
            replies: false,
            about: String::new(),
            about_approved: None,
            reply_gap_s: 60,
        }
    }
}

fn hash(s: &str) -> String {
    Sha256::digest(s.trim().as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

impl SmartSettings {
    pub fn approved(&self) -> bool {
        !self.about.trim().is_empty() && self.about_approved.as_deref() == Some(hash(&self.about).as_str())
    }
}

/// What a person changes (`PUT /api/v1/smart`); missing fields stay as they are.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmartPatch {
    pub moderation: Option<ModMode>,
    pub act_timeout_s: Option<u32>,
    pub rules: Option<String>,
    pub replies: Option<bool>,
    pub about: Option<String>,
    pub reply_gap_s: Option<u32>,
}

/// What a suggestion is now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionState {
    Suggested,
    Done,
    Undone,
    Dismissed,
}

/// One flagged message.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Suggestion {
    /// The chat event's id.
    pub id: String,
    pub platform: Platform,
    pub user_id: Option<String>,
    pub login: Option<String>,
    pub name: String,
    pub text: String,
    /// The platform's message id (for Delete).
    pub message_id: Option<String>,
    /// What the check suggests: `delete`, `timeout` or `ban`.
    pub action: String,
    pub category: String,
    pub reason: String,
    pub confidence: f64,
    pub at: i64,
    pub state: SuggestionState,
    /// What was done (`delete`, `timeout`, `ban`), by whom (`smart` on its own, else `you`).
    pub done: Option<String>,
    pub by: Option<String>,
    pub seconds: Option<u32>,
    pub error: Option<String>,
}

/// One reply the bot sent.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SentReply {
    pub at: i64,
    pub platform: Platform,
    pub name: String,
    pub question: String,
    pub reply: String,
}

#[derive(Debug, Clone)]
struct Waiting {
    event_id: String,
    platform: Platform,
    user_id: Option<String>,
    login: Option<String>,
    name: String,
    text: String,
    message_id: Option<String>,
    first_time: bool,
    at: i64,
}

/// In memory, in the engine.
#[derive(Debug, Default)]
pub(crate) struct Smart {
    settings: SmartSettings,
    /// Pro with `features.smart` (re-read every tick, from memory).
    on: bool,
    queue: VecDeque<Waiting>,
    log: VecDeque<Suggestion>,
    replies: VecDeque<SentReply>,
    /// No moderation check / reply until then (the relay said to wait).
    mod_paused_until: i64,
    reply_paused_until: i64,
    last_error: Option<Value>,
    reply_last: HashMap<Platform, i64>,
    reply_viewer: HashMap<String, i64>,
    /// One moderation check in flight at a time.
    checking: bool,
}

// ── The local first look ─────────────────────────────────────────────────────────────

/// Word stems that are often abuse in English, Hindi and Hinglish (spellings folded: 0→o,
/// 1→i, 3→e, 4→a, 5→s, 7→t, @→a, $→s, repeats squeezed). Only a reason to ask, never a verdict.
const STEMS: &[&str] = &[
    "chut", "chtiy", "cutiy", "bhenc", "behenc", "madarc", "maderc", "bsdk", "bhosd", "gaand", "randi", "harami",
    "kamina", "kutta", "kutti", "lund", "lavd", "lawd", "chakka", "hijra", "chamar", "bhangi", "katua", "mulla",
    "suar", "fuck", "bitch", "slut", "whore", "nigg", "faggot", "retard", "killyou", "doxx",
];
/// Short ones only as a whole word (dots and stars inside a word are ignored: "m.c").
const WORDS: &[&str] = &["mc", "bc", "mkc", "bkl", "tmkc", "chu", "lodu", "lode", "loda", "gand", "rape", "kys", "fag"];

fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = '\0';
    for c in text.chars().flat_map(char::to_lowercase) {
        let c = match c {
            '0' => 'o',
            '1' | '!' => 'i',
            '3' => 'e',
            '4' | '@' => 'a',
            '5' | '$' => 's',
            '7' => 't',
            c => c,
        };
        if c.is_alphanumeric() && c != last {
            out.push(c);
            last = c;
        } else if c.is_whitespace() {
            last = '\0';
        }
    }
    out
}

/// Whether a message is worth asking about (cheap; no network).
pub fn worth_checking(text: &str, first_time: bool) -> bool {
    let t = text.trim();
    if t.chars().count() < 2 {
        return false;
    }
    if first_time {
        return true;
    }
    let lower = t.to_lowercase();
    if ["http://", "https://", "www.", ".com", ".gg/", "bit.ly", "t.me/", "wa.me/", "discord.gg"]
        .iter()
        .any(|l| lower.contains(l))
    {
        return true;
    }
    let mut digits = 0;
    let mut longest = 0;
    for c in t.chars() {
        if c.is_ascii_digit() {
            digits += 1;
            longest = longest.max(digits);
        } else if !matches!(c, ' ' | '-' | '.') {
            digits = 0;
        }
    }
    if longest >= 7 {
        return true;
    }
    let folded = fold(t);
    if STEMS.iter().any(|s| folded.contains(s)) {
        return true;
    }
    let joined: String = lower.chars().filter(|c| !matches!(c, '.' | '*' | '_' | '-')).collect();
    if joined.split(|c: char| !c.is_alphanumeric()).any(|w| WORDS.contains(&w)) {
        return true;
    }
    let letters: Vec<char> = t.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.len() >= 12 && letters.iter().all(|c| !c.is_lowercase()) {
        return true;
    }
    let mut run = 1;
    let mut prev = '\0';
    for c in t.chars() {
        run = if c == prev { run + 1 } else { 1 };
        if run >= 8 {
            return true;
        }
        prev = c;
    }
    t.chars().count() > 250
}

const QUESTION_WORDS: &[&str] = &[
    "what", "when", "where", "why", "how", "who", "which", "is", "are", "do", "does", "can", "will", "kya", "kab",
    "kaise", "kaisa", "kaisi", "kyu", "kyun", "kyon", "kahan", "kaha", "kaun", "kon", "kitna", "kitne", "kitni",
    "konsa", "konsi",
];

/// Whether a chat line reads as a question to the stream (not a command).
pub fn looks_like_question(text: &str) -> bool {
    let t = text.trim();
    let n = t.chars().count();
    if !(6..=300).contains(&n) || t.starts_with(['!', '/']) {
        return false;
    }
    if t.contains('?') {
        return true;
    }
    let first = t.split_whitespace().next().unwrap_or("").to_lowercase();
    let first = first.trim_matches(|c: char| !c.is_alphanumeric());
    QUESTION_WORDS.contains(&first) && t.split_whitespace().count() >= 3
}

// ── Errors ───────────────────────────────────────────────────────────────────────────

fn refused(status: u16, code: &'static str, message: &str, hint: &str) -> EngineError {
    EngineError::Refused { status, code, message: message.into(), hint: hint.into(), retry_after: None }
}

fn not_pro() -> EngineError {
    refused(
        402,
        "pro_required",
        "This runs on Ferrocast's servers and is part of Pro.",
        "See Settings › Pro. Everything free in Ferrocast stays free.",
    )
}

fn off() -> EngineError {
    refused(
        503,
        "smart_off",
        "Smart features aren't switched on on Ferrocast's servers yet.",
        "Nothing to do: everything else works as usual, and these turn on by themselves once they're set up.",
    )
}

const GONE: EngineError = EngineError::Missing {
    code: "not_found",
    message: "That suggestion is gone.",
    hint: "Ferrocast keeps the last 200 in this session. Reload the page.",
};

fn error_json(e: &EngineError) -> Value {
    match e {
        EngineError::Refused { status, code, message, hint, retry_after } => {
            json!({ "status": status, "code": code, "message": message, "hint": hint, "retry_after": retry_after, "at": now().0 })
        }
        other => {
            json!({ "status": 502, "code": "relay_unreachable", "message": other.to_string(), "hint": "Ferrocast tries again by itself.", "at": now().0 })
        }
    }
}

/// How long to wait after a refusal: the relay's `Retry-After`, else a minute; never past a day.
fn pause_for(e: &EngineError) -> i64 {
    let secs = match e {
        EngineError::Refused { retry_after: Some(s), .. } => *s,
        EngineError::Refused { status: 402 | 503, .. } => 15 * 60,
        _ => 60,
    };
    i64::try_from(secs.min(86_400)).unwrap_or(60) * 1000
}

// ── The engine side ──────────────────────────────────────────────────────────────────

impl Engine {
    fn smart(&self) -> std::sync::MutexGuard<'_, Smart> {
        self.inner.smart.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) async fn load_smart(&self) {
        let s = match self.db(|s| repo::settings::get::<SmartSettings>(s, SETTINGS_KEY)).await {
            Ok(s) => s.unwrap_or_default(),
            Err(e) => {
                tracing::warn!(error = %e, "couldn't read the smart settings; they start off");
                SmartSettings::default()
            }
        };
        let on = self.pro_has(|f| f.smart).await;
        let mut m = self.smart();
        m.settings = s;
        m.on = on;
    }

    /// `GET /api/v1/smart`.
    pub async fn smart_view(&self) -> Value {
        let pro = self.is_pro().await;
        let available = self.relay_has(|f| f.smart).await;
        let quotas = self.pro_quotas(&["smart_moderation", "smart_replies", "smart_recaps"]).await;
        let m = self.smart();
        let n = now().0;
        json!({
            "settings": {
                "moderation": m.settings.moderation, "act_timeout_s": m.settings.act_timeout_s,
                "rules": m.settings.rules, "replies": m.settings.replies, "about": m.settings.about,
                "reply_gap_s": m.settings.reply_gap_s,
            },
            "about_approved": m.settings.approved(),
            "pro": pro,
            "available": available,
            "active": pro && available,
            "quotas": quotas,
            "moderation_paused_until": (m.mod_paused_until > n).then_some(m.mod_paused_until),
            "replies_paused_until": (m.reply_paused_until > n).then_some(m.reply_paused_until),
            "last_error": m.last_error,
            "waiting": m.queue.len(),
        })
    }

    /// `PUT /api/v1/smart`: changes what's given; a changed `about` needs approving again.
    pub async fn smart_save(&self, patch: SmartPatch) -> Result<Value, EngineError> {
        let mut s = self.smart().settings.clone();
        if let Some(m) = patch.moderation {
            s.moderation = m;
        }
        if let Some(t) = patch.act_timeout_s {
            if !(10..=600).contains(&t) {
                return Err(EngineError::Invalid("act_timeout_s: 10 seconds to 10 minutes.".into()));
            }
            s.act_timeout_s = t;
        }
        if let Some(r) = patch.rules {
            let r = r.trim().to_string();
            if r.chars().count() > MAX_RULES {
                return Err(EngineError::Invalid(format!("rules: at most {MAX_RULES} characters.")));
            }
            s.rules = r;
        }
        if let Some(r) = patch.replies {
            s.replies = r;
        }
        if let Some(a) = patch.about {
            let a = a.trim().to_string();
            if a.chars().count() > MAX_ABOUT {
                return Err(EngineError::Invalid(format!("about: at most {MAX_ABOUT} characters.")));
            }
            if a != s.about {
                s.about = a;
                s.about_approved = None;
            }
        }
        if let Some(g) = patch.reply_gap_s {
            if !(30..=3_600).contains(&g) {
                return Err(EngineError::Invalid("reply_gap_s: 30 seconds to an hour.".into()));
            }
            s.reply_gap_s = g;
        }
        let saved = s.clone();
        self.db(move |st| repo::settings::set(st, SETTINGS_KEY, &saved)).await?;
        {
            let mut m = self.smart();
            if s.moderation == ModMode::Off {
                m.queue.clear();
            }
            m.settings = s;
        }
        Ok(self.smart_view().await)
    }

    /// `POST /api/v1/smart/about/approve {about}`: the streamer approves exactly this text
    /// (it must be the saved one, so an old page can't approve something else).
    pub async fn smart_approve(&self, about: &str) -> Result<Value, EngineError> {
        let mut s = self.smart().settings.clone();
        if s.about.trim().is_empty() {
            return Err(EngineError::Invalid("Write the facts for smart replies first.".into()));
        }
        if about.trim() != s.about.trim() {
            return Err(refused(
                409,
                "about_changed",
                "The facts changed since this page loaded.",
                "Reload, read them again, then approve.",
            ));
        }
        s.about_approved = Some(hash(&s.about));
        let saved = s.clone();
        self.db(move |st| repo::settings::set(st, SETTINGS_KEY, &saved)).await?;
        self.smart().settings = s;
        Ok(self.smart_view().await)
    }

    /// `GET /api/v1/smart/moderation` → `{items: [...]}` newest first, and the smart replies.
    pub fn smart_log(&self) -> Value {
        let m = self.smart();
        json!({ "items": m.log.iter().rev().collect::<Vec<_>>(), "replies": m.replies.iter().rev().collect::<Vec<_>>() })
    }

    fn smart_update(&self, id: &str, f: impl FnOnce(&mut Suggestion)) -> Option<Suggestion> {
        let s = {
            let mut m = self.smart();
            let s = m.log.iter_mut().find(|s| s.id == id)?;
            f(s);
            s.clone()
        };
        self.send_live(LiveFrame::new("smart_mod", &s));
        Some(s)
    }

    /// `POST /api/v1/smart/moderation/{id}/apply {action?, seconds?}`: does what the
    /// suggestion says (or `action`), as the streamer.
    pub async fn smart_apply(
        &self,
        id: &str,
        action: Option<&str>,
        seconds: Option<u32>,
    ) -> Result<Suggestion, EngineError> {
        let s = self.smart().log.iter().find(|s| s.id == id).cloned().ok_or(GONE)?;
        if s.state == SuggestionState::Done {
            return Err(EngineError::Conflict("That's already done.".into()));
        }
        let action = action.unwrap_or(&s.action).to_string();
        let secs = seconds.unwrap_or(600);
        let result = match action.as_str() {
            "delete" => match &s.message_id {
                Some(mid) => self.delete_message(s.platform, mid).await,
                None => Err(EngineError::Invalid("This platform didn't give that message an id to delete.".into())),
            },
            "timeout" | "ban" => match &s.user_id {
                None => Err(EngineError::Invalid("This platform didn't say who sent it.".into())),
                Some(uid) if action == "timeout" => {
                    self.timeout(s.platform, uid, secs, Some(&format!("Smart moderation: {}", s.reason))).await
                }
                Some(uid) => {
                    let login = s.login.clone().unwrap_or_else(|| s.name.clone());
                    self.ban(
                        s.platform,
                        uid,
                        &login,
                        Some(&s.name),
                        Some(format!("Smart moderation: {}", s.reason)),
                        false,
                    )
                    .await
                    .and_then(|o| match o.failed.first() {
                        Some(f) => Err(EngineError::Unavailable(f.reason.clone())),
                        None => Ok(()),
                    })
                }
            },
            _ => return Err(EngineError::Invalid("action: delete, timeout or ban.".into())),
        };
        result?;
        self.smart_update(id, |x| {
            x.state = SuggestionState::Done;
            x.done = Some(action.clone());
            x.by = Some("you".into());
            x.seconds = (action == "timeout").then_some(secs);
            x.error = None;
        })
        .ok_or(GONE)
    }

    /// `POST /api/v1/smart/moderation/{id}/undo`: lifts a timeout or ban. A deleted message
    /// can't come back (409).
    pub async fn smart_undo(&self, id: &str) -> Result<Suggestion, EngineError> {
        let s = self.smart().log.iter().find(|s| s.id == id).cloned().ok_or(GONE)?;
        match (s.state, s.done.as_deref(), &s.user_id) {
            (SuggestionState::Done, Some("timeout" | "ban"), Some(uid)) => {
                self.unban(s.platform, uid).await?;
            }
            (SuggestionState::Done, Some("delete"), _) => {
                return Err(refused(
                    409,
                    "cant_undo",
                    "A deleted message can't be brought back.",
                    "The person can still chat; nothing else was done to them.",
                ));
            }
            _ => return Err(EngineError::Conflict("There's nothing to undo here.".into())),
        }
        self.smart_update(id, |x| {
            x.state = SuggestionState::Undone;
            x.error = None;
        })
        .ok_or(GONE)
    }

    /// `POST /api/v1/smart/moderation/{id}/dismiss`.
    pub fn smart_dismiss(&self, id: &str) -> Result<Suggestion, EngineError> {
        self.smart_update(id, |x| {
            if x.state == SuggestionState::Suggested {
                x.state = SuggestionState::Dismissed;
            }
        })
        .ok_or(GONE)
    }

    /// A chat message that passed the filters (from the pipeline; cheap, never waits).
    pub(crate) fn smart_saw(&self, e: &Event) {
        let EventKind::ChatMessage { actor, content, .. } = &e.kind else { return };
        if e.origin != Origin::Live || e.channel.is_some() {
            return;
        }
        let exempt = {
            let st = self.state();
            actor.roles.0 & st.filter.exempt_roles.0 != 0
        };
        if exempt || actor.roles.contains(Roles::BROADCASTER) || actor.roles.contains(Roles::MODERATOR) {
            return;
        }
        let mut m = self.smart();
        if !m.on || m.settings.moderation == ModMode::Off || m.mod_paused_until > now().0 {
            return;
        }
        let first = actor.roles.contains(Roles::FIRST_TIME);
        if !worth_checking(&content.text, first) || m.queue.len() >= QUEUE_MAX {
            return;
        }
        m.queue.push_back(Waiting {
            event_id: e.id.clone(),
            platform: e.platform,
            user_id: actor.id.clone(),
            login: actor.login.clone(),
            name: actor.display_name.clone(),
            text: content.text.chars().take(300).collect(),
            message_id: e.source_id.clone().filter(|s| !s.is_empty()),
            first_time: first,
            at: now().0,
        });
    }

    /// One round of the task: Pro from memory, then a batch if one is due.
    pub(crate) async fn smart_round(&self) {
        let on = self.pro_has(|f| f.smart).await;
        let batch: Vec<Waiting> = {
            let mut m = self.smart();
            m.on = on;
            if !on || m.settings.moderation == ModMode::Off {
                m.queue.clear();
                return;
            }
            let n = now().0;
            let due = m.queue.len() >= BATCH || m.queue.front().is_some_and(|w| n - w.at >= BATCH_WAIT_MS);
            if !due || m.checking || m.mod_paused_until > n {
                return;
            }
            m.checking = true;
            let k = m.queue.len().min(BATCH);
            m.queue.drain(..k).collect()
        };
        let rules = self.smart().settings.rules.clone();
        let body = json!({
            "messages": batch.iter().map(|w| json!({ "id": w.event_id, "text": w.text, "user": w.name, "first_time": w.first_time })).collect::<Vec<_>>(),
            "rules": (!rules.is_empty()).then_some(rules),
        });
        let res = self.pro_call(reqwest::Method::POST, "/v1/pro/smart/moderate", Some(body)).await;
        self.smart().checking = false;
        let results = match res {
            Ok(v) => v["results"].as_array().cloned().unwrap_or_default(),
            Err(e) => {
                tracing::debug!(error = %e, "smart moderation check didn't run");
                let mut m = self.smart();
                m.mod_paused_until = now().0 + pause_for(&e);
                m.last_error = Some(error_json(&e));
                return;
            }
        };
        self.smart().last_error = None;
        for r in results {
            let Some(w) = batch.iter().find(|w| r["id"].as_str() == Some(w.event_id.as_str())) else { continue };
            let action = r["action"].as_str().unwrap_or("none");
            let confidence = r["confidence"].as_f64().unwrap_or(0.0);
            if !matches!(action, "delete" | "timeout" | "ban") || confidence < SUGGEST_CONFIDENCE {
                continue;
            }
            let s = Suggestion {
                id: w.event_id.clone(),
                platform: w.platform,
                user_id: w.user_id.clone(),
                login: w.login.clone(),
                name: w.name.clone(),
                text: w.text.clone(),
                message_id: w.message_id.clone(),
                action: action.into(),
                category: r["category"].as_str().unwrap_or("other").chars().take(20).collect(),
                reason: r["reason"].as_str().unwrap_or("").chars().take(120).collect(),
                confidence,
                at: now().0,
                state: SuggestionState::Suggested,
                done: None,
                by: None,
                seconds: None,
                error: None,
            };
            let (mode, secs) = {
                let mut m = self.smart();
                m.log.push_back(s.clone());
                while m.log.len() > LOG_KEEP {
                    m.log.pop_front();
                }
                (m.settings.moderation, m.settings.act_timeout_s)
            };
            self.send_live(LiveFrame::new("smart_mod", &s));
            // On its own: only a timeout (a ban suggestion too), only when very clear, with Undo.
            if mode == ModMode::Act
                && confidence >= ACT_CONFIDENCE
                && matches!(action, "timeout" | "ban")
                && let Some(uid) = &w.user_id
            {
                let why = format!("Smart moderation: {}", s.reason);
                match self.timeout(w.platform, uid, secs, Some(&why)).await {
                    Ok(()) => {
                        self.smart_update(&s.id, |x| {
                            x.state = SuggestionState::Done;
                            x.done = Some("timeout".into());
                            x.by = Some("smart".into());
                            x.seconds = Some(secs);
                        });
                    }
                    Err(e) => {
                        self.smart_update(&s.id, |x| x.error = Some(format!("Couldn't time out: {e}")));
                    }
                }
            }
        }
    }

    /// After commands and auto-replies left a live chat line alone (from the pipeline):
    /// a smart reply if it's a question and every limit allows it. The answer is fetched
    /// and sent in the background.
    pub(crate) fn smart_reply_saw(&self, e: &Event) {
        let EventKind::ChatMessage { actor, content, .. } = &e.kind else { return };
        if e.origin != Origin::Live || e.channel.is_some() || !looks_like_question(&content.text) {
            return;
        }
        let (bot_on, panic, live, channel) = {
            let st = self.state();
            let channel =
                st.connectors.get(platform_key(e.platform)).and_then(|c| c.account.as_ref()).map(|a| a.name.clone());
            (st.settings.bot_enabled, st.panic_on, st.live.get(&e.platform).copied().unwrap_or(false), channel)
        };
        let at = now();
        if !bot_on || panic || !live || self.is_bot_message(e.platform, actor, &content.text, at) {
            return;
        }
        let viewer = format!("{}:{}", platform_key(e.platform), actor.id.as_deref().unwrap_or(&actor.display_name));
        let about = {
            let mut m = self.smart();
            let n = at.0;
            let gap = i64::from(m.settings.reply_gap_s) * 1000;
            if !m.on || !m.settings.replies || !m.settings.approved() || m.reply_paused_until > n {
                return;
            }
            if m.reply_last.get(&e.platform).is_some_and(|t| n - t < gap)
                || m.reply_viewer.get(&viewer).is_some_and(|t| n - t < VIEWER_GAP_MS)
            {
                return;
            }
            m.reply_last.insert(e.platform, n);
            m.reply_viewer.insert(viewer, n);
            m.reply_viewer.retain(|_, t| n - *t < VIEWER_GAP_MS);
            m.settings.about.clone()
        };
        let engine = self.clone();
        let platform = e.platform;
        let reply_to = e.source_id.clone();
        let question = content.text.clone();
        let name = actor.display_name.clone();
        tokio::spawn(async move {
            let saved = engine.stream_info_saved().await.map(|s| s.history).unwrap_or_default();
            let game = saved.iter().find_map(|h| h.info.category.as_ref().map(|c| c.name.clone()));
            let title = saved.iter().find_map(|h| h.info.title.clone());
            let body = json!({ "question": question, "about": about, "user": name, "channel": channel, "title": title, "game": game });
            match engine.pro_call(reqwest::Method::POST, "/v1/pro/smart/reply", Some(body)).await {
                Ok(v) => {
                    engine.smart().last_error = None;
                    let Some(reply) = v["reply"].as_str().map(String::from) else { return };
                    {
                        let mut m = engine.smart();
                        m.replies.push_back(SentReply { at: now().0, platform, name, question, reply: reply.clone() });
                        while m.replies.len() > REPLIES_KEEP {
                            m.replies.pop_front();
                        }
                    }
                    engine.spawn_action(platform, "send a smart reply", move |c| {
                        Box::pin(async move { c.send_chat(&reply, reply_to.as_deref()).await })
                    });
                }
                Err(err) => {
                    tracing::debug!(error = %err, "smart reply didn't run");
                    let mut m = engine.smart();
                    if matches!(err, EngineError::Refused { status: 402 | 429 | 503, .. }) {
                        m.reply_paused_until = now().0 + pause_for(&err);
                    }
                    m.last_error = Some(error_json(&err));
                }
            }
        });
    }

    /// `POST /api/v1/recap/summary {since?}` → `{started_at, summary, at}`: a written recap
    /// of this (or the last) stream, kept for it.
    pub async fn smart_recap(&self, since: Option<i64>) -> Result<Value, EngineError> {
        if !self.is_pro().await {
            return Err(not_pro());
        }
        if !self.relay_has(|f| f.smart).await {
            return Err(off());
        }
        let r = self.recap(since).await?;
        let Some(started) = r.started_at else {
            return Err(refused(
                409,
                "no_stream",
                "There's no stream to recap yet.",
                "Stream first, then ask for a recap.",
            ));
        };
        let end = r.ended_at.map_or(now().0, |m| m.0);
        let money = |m: &ferrocast_core::money::Money| {
            let whole = if matches!(m.currency.as_str(), "JPY" | "KRW") { 1 } else { 100 };
            if whole == 1 {
                format!("{} {}", m.currency, m.minor)
            } else {
                format!("{} {}.{:02}", m.currency, m.minor / 100, m.minor % 100)
            }
        };
        let saved = self.stream_info_saved().await.map(|s| s.history).unwrap_or_default();
        let body = json!({
            "minutes": (end - started.0).max(0) / 60_000,
            "title": saved.iter().find_map(|h| h.info.title.clone()),
            "game": saved.iter().find_map(|h| h.info.category.as_ref().map(|c| c.name.clone())),
            "peak_viewers": r.totals.peak_viewers,
            "follows": r.totals.follows,
            "subs": r.totals.subs,
            "messages": r.totals.messages,
            "bits": r.totals.bits,
            "tips": r.totals.tips.iter().map(money).collect::<Vec<_>>(),
            "raids": r.raiders.iter().map(|x| format!("{} ({} viewers)", x.name, x.viewers)).collect::<Vec<_>>(),
            "thanks": r.thanks.iter().take(15).collect::<Vec<_>>(),
        });
        let v = self.pro_call(reqwest::Method::POST, "/v1/pro/smart/recap", Some(body)).await?;
        let summary = v["summary"].as_str().unwrap_or("").to_string();
        let out = json!({ "started_at": started.0, "summary": summary, "at": now().0 });
        let keep = out.clone();
        if let Err(e) = self.db(move |s| repo::settings::set(s, RECAP_KEY, &keep)).await {
            tracing::warn!(error = %e, "couldn't keep the written recap");
        }
        Ok(out)
    }

    /// `GET /api/v1/recap/summary` → the last written recap, or `{summary: null}`.
    pub async fn smart_recap_saved(&self) -> Result<Value, EngineError> {
        let v = self.db(|s| repo::settings::get::<Value>(s, RECAP_KEY)).await?;
        Ok(v.unwrap_or_else(|| json!({ "started_at": null, "summary": null, "at": null })))
    }
}

/// The moderation task: a round every two seconds, never in the pipeline's way.
pub(crate) async fn run(engine: Engine) {
    loop {
        tokio::time::sleep(TICK).await;
        engine.smart_round().await;
    }
}

#[cfg(test)]
mod tests;
