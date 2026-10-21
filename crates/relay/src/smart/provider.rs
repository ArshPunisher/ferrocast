//! The language model behind smart moderation, smart replies and the stream recap: one call
//! that turns a system and a user text into an answer. Free tiers only:
//!
//! - **Cloudflare Workers AI** (`CLOUDFLARE_AI_ACCOUNT_ID` + `CLOUDFLARE_AI_TOKEN`), the
//!   default: its free allocation is 10,000 Neurons a day and, on the Workers Free plan, it
//!   stops instead of billing. Cloudflare doesn't train on what it's sent.
//! - **Google Gemini** (`GEMINI_API_KEY`): the backup once Cloudflare rests or is down, or the
//!   provider when Cloudflare isn't set. The free tier of the Gemini API. The key must come from a Google project **without billing** (a project with
//!   billing is charged), and on the free tier Google may use what it's sent to improve its
//!   products, so viewers' chat goes to Google: Cloudflare is the better choice.
//!
//! Keys travel in headers, never in a URL, and never appear in `Debug`, an error or a log.
//! Neither the prompts nor the answers are logged.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Value, json};

/// One call: the instructions, the data, how long the answer may be, and whether it's JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub system: String,
    pub user: String,
    pub max_tokens: u32,
    pub json: bool,
}

/// Why the model didn't answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SmartError {
    /// The provider's free allocation or rate limit is used up for now.
    #[error("the model's free allowance is used up for now")]
    Quota,
    /// Down, slow, a refused key, or an answer we can't use. Never holds the text.
    #[error("the model is unavailable: {0}")]
    Unavailable(String),
}

pub type SmartFuture<'a> = Pin<Box<dyn Future<Output = Result<String, SmartError>> + Send + 'a>>;

pub trait SmartProvider: Send + Sync + std::fmt::Debug {
    /// `cloudflare`, `gemini` or `fake`.
    fn name(&self) -> &'static str;
    fn complete<'a>(&'a self, p: &'a Prompt) -> SmartFuture<'a>;
}

const TIMEOUT: Duration = Duration::from_secs(12);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

fn http() -> reqwest::Client {
    reqwest::Client::builder().timeout(TIMEOUT).connect_timeout(CONNECT_TIMEOUT).build().unwrap_or_default()
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn send_error(e: &reqwest::Error) -> SmartError {
    SmartError::Unavailable(if e.is_timeout() { "timed out".into() } else { "unreachable".into() })
}

// ── Cloudflare Workers AI ────────────────────────────────────────────────────────────

pub const CLOUDFLARE_BASE: &str = "https://api.cloudflare.com/client/v4";
/// A small multilingual model: cheap in Neurons, reads Hindi and Hinglish well enough.
pub const CLOUDFLARE_MODEL: &str = "@cf/meta/llama-3.1-8b-instruct";

pub struct Cloudflare {
    account: String,
    token: String,
    model: String,
    base: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for Cloudflare {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cloudflare").field("token", &"<redacted>").field("model", &self.model).finish()
    }
}

impl Cloudflare {
    pub fn new(account: String, token: String, model: Option<String>, base: Option<String>) -> Cloudflare {
        Cloudflare {
            account,
            token,
            model: model.unwrap_or_else(|| CLOUDFLARE_MODEL.into()),
            base: base.unwrap_or_else(|| CLOUDFLARE_BASE.into()),
            http: http(),
        }
    }

    /// From `CLOUDFLARE_AI_ACCOUNT_ID` and `CLOUDFLARE_AI_TOKEN` (`SMART_MODEL` picks another
    /// Workers AI model; `CLOUDFLARE_AI_URL` is for tests). `None` unless both are set.
    pub fn from_env() -> Option<Cloudflare> {
        Some(Cloudflare::new(
            env("CLOUDFLARE_AI_ACCOUNT_ID")?,
            env("CLOUDFLARE_AI_TOKEN")?,
            env("SMART_MODEL"),
            env("CLOUDFLARE_AI_URL"),
        ))
    }

    async fn call(&self, p: &Prompt) -> Result<String, SmartError> {
        let url = format!("{}/accounts/{}/ai/run/{}", self.base, self.account, self.model);
        let body = json!({
            "messages": [{ "role": "system", "content": p.system }, { "role": "user", "content": p.user }],
            "max_tokens": p.max_tokens,
            "temperature": 0.2,
        });
        let res = self.http.post(&url).bearer_auth(&self.token).json(&body).send().await.map_err(|e| send_error(&e))?;
        let status = res.status().as_u16();
        let v: Value = res.json().await.unwrap_or(Value::Null);
        if status != 200 || v["success"] == false {
            // Cloudflare's daily free allocation answers 429 (or error 3036 / 4006).
            let codes: Vec<i64> = v["errors"]
                .as_array()
                .map(|a| a.iter().filter_map(|e| e["code"].as_i64()).collect())
                .unwrap_or_default();
            return Err(match status {
                429 => SmartError::Quota,
                _ if codes.iter().any(|c| matches!(c, 3036 | 4006)) => SmartError::Quota,
                401 | 403 => SmartError::Unavailable("Cloudflare refused the token".into()),
                s => SmartError::Unavailable(format!("Cloudflare answered {s}")),
            });
        }
        match &v["result"]["response"] {
            Value::String(s) => Ok(s.clone()),
            // JSON-mode models answer an object.
            o @ (Value::Object(_) | Value::Array(_)) => Ok(o.to_string()),
            _ => Err(SmartError::Unavailable("no answer in Cloudflare's reply".into())),
        }
    }
}

impl SmartProvider for Cloudflare {
    fn name(&self) -> &'static str {
        "cloudflare"
    }

    fn complete<'a>(&'a self, p: &'a Prompt) -> SmartFuture<'a> {
        Box::pin(self.call(p))
    }
}

// ── Google Gemini (free tier) ────────────────────────────────────────────────────────

pub const GEMINI_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
pub const GEMINI_MODEL: &str = "gemini-2.5-flash-lite";

pub struct Gemini {
    key: String,
    model: String,
    base: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for Gemini {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gemini").field("key", &"<redacted>").field("model", &self.model).finish()
    }
}

impl Gemini {
    pub fn new(key: String, model: Option<String>, base: Option<String>) -> Gemini {
        Gemini {
            key,
            model: model.unwrap_or_else(|| GEMINI_MODEL.into()),
            base: base.unwrap_or_else(|| GEMINI_BASE.into()),
            http: http(),
        }
    }

    /// From `GEMINI_API_KEY` (`SMART_GEMINI_MODEL`, `GEMINI_URL` for tests).
    pub fn from_env() -> Option<Gemini> {
        Some(Gemini::new(env("GEMINI_API_KEY")?, env("SMART_GEMINI_MODEL"), env("GEMINI_URL")))
    }

    async fn call(&self, p: &Prompt) -> Result<String, SmartError> {
        let url = format!("{}/models/{}:generateContent", self.base, self.model);
        let mut config = json!({ "temperature": 0.2, "maxOutputTokens": p.max_tokens });
        if p.json {
            config["responseMimeType"] = json!("application/json");
        }
        let body = json!({
            "systemInstruction": { "parts": [{ "text": p.system }] },
            "contents": [{ "role": "user", "parts": [{ "text": p.user }] }],
            "generationConfig": config,
        });
        let res = self
            .http
            .post(&url)
            .header("x-goog-api-key", &self.key)
            .json(&body)
            .send()
            .await
            .map_err(|e| send_error(&e))?;
        let status = res.status().as_u16();
        let v: Value = res.json().await.unwrap_or(Value::Null);
        if status != 200 {
            return Err(match status {
                429 => SmartError::Quota,
                400 | 401 | 403 => SmartError::Unavailable("Google refused the key or the request".into()),
                s => SmartError::Unavailable(format!("Google answered {s}")),
            });
        }
        let parts = v.pointer("/candidates/0/content/parts").and_then(Value::as_array);
        let text: String = parts.map(|ps| ps.iter().filter_map(|p| p["text"].as_str()).collect()).unwrap_or_default();
        if text.trim().is_empty() {
            return Err(SmartError::Unavailable("no answer in Google's reply".into()));
        }
        Ok(text)
    }
}

impl SmartProvider for Gemini {
    fn name(&self) -> &'static str {
        "gemini"
    }

    fn complete<'a>(&'a self, p: &'a Prompt) -> SmartFuture<'a> {
        Box::pin(self.call(p))
    }
}

// ── A fake for tests ─────────────────────────────────────────────────────────────────

/// Answers from a list (then the default), records every prompt.
#[derive(Debug, Default)]
pub struct FakeSmart {
    pub answers: Mutex<VecDeque<Result<String, SmartError>>>,
    pub default: Mutex<Option<String>>,
    pub calls: Mutex<Vec<Prompt>>,
}

impl FakeSmart {
    pub fn answering(text: &str) -> FakeSmart {
        FakeSmart { default: Mutex::new(Some(text.into())), ..FakeSmart::default() }
    }

    pub fn then(&self, r: Result<String, SmartError>) {
        self.answers.lock().unwrap_or_else(|p| p.into_inner()).push_back(r);
    }

    pub fn calls(&self) -> Vec<Prompt> {
        self.calls.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

impl SmartProvider for FakeSmart {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn complete<'a>(&'a self, p: &'a Prompt) -> SmartFuture<'a> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).push(p.clone());
        let next = self.answers.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
        let out = next.unwrap_or_else(|| {
            self.default
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .ok_or_else(|| SmartError::Unavailable("no answer set".into()))
        });
        Box::pin(async move { out })
    }
}
