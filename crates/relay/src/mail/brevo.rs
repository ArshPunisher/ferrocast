//! Brevo's transactional email API: `POST https://api.brevo.com/v3/smtp/email` with the key
//! from `BREVO_API_KEY` in the `api-key` header (never in a URL, never logged or shown by
//! `Debug`). The sending domain (mail.clearcraftlabs.com) is authenticated in Brevo.
//!
//! Brevo's answers ([`classify`]):
//! - 201 sent.
//! - 400 about the recipient (`invalid_parameter` / `missing_parameter` naming `to`, the
//!   email or the recipient, or a blocked / blacklisted contact) → [`MailError::Rejected`]:
//!   the address is paused. Any other 400 (the sender, the body) → [`MailError::Misconfigured`].
//! - 402, a 403 about credits or a limit, 429 → [`MailError::Limited`]: nothing more today.
//! - Other 401 / 403 / 404 / 405 / 4xx → [`MailError::Misconfigured`].
//! - No connection, 503 → [`MailError::Unavailable`] (not accepted: try again).
//! - A read timeout, 500 / 502 / 504 / other 5xx → [`MailError::Uncertain`] (it may have gone).
//!
//! Brevo's own message can quote the address, so it's only read to classify the answer and
//! then dropped: the error keeps the status and Brevo's `code`.

use std::time::Duration;

use serde_json::{Map, Value, json};

use super::mailer::{Email, MailError, Mailer, SendFuture, Sender};

pub const DEFAULT_URL: &str = "https://api.brevo.com/v3/smtp/email";
const TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

pub struct BrevoMailer {
    key: String,
    url: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for BrevoMailer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrevoMailer").field("key", &"<redacted>").field("url", &self.url).finish()
    }
}

impl BrevoMailer {
    /// `url` is Brevo's endpoint in production (tests point it at a fake).
    pub fn new(key: String, url: String) -> BrevoMailer {
        let http =
            reqwest::Client::builder().timeout(TIMEOUT).connect_timeout(CONNECT_TIMEOUT).build().unwrap_or_default();
        BrevoMailer { key, url, http }
    }

    /// The same with another overall timeout (tests: a slow answer is a lost one).
    pub fn with_timeout(mut self, timeout: Duration) -> BrevoMailer {
        self.http = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(CONNECT_TIMEOUT.min(timeout))
            .build()
            .unwrap_or_default();
        self
    }

    /// From `BREVO_API_KEY` (and `BREVO_API_URL`, only for tests); `None` without a key.
    pub fn from_env() -> Option<BrevoMailer> {
        let env = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let key = env("BREVO_API_KEY")?;
        Some(BrevoMailer::new(key, env("BREVO_API_URL").unwrap_or_else(|| DEFAULT_URL.into())))
    }

    /// The JSON Brevo gets for one email.
    pub fn body(from: &Sender, e: &Email) -> Value {
        let headers: Map<String, Value> =
            e.headers.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
        let mut body = json!({
            "sender": { "name": from.name, "email": from.email },
            "to": [{ "email": e.to }],
            "subject": e.subject,
            "htmlContent": e.html,
            "textContent": e.text,
            "tags": [e.tag],
        });
        // Brevo refuses an empty `headers` ("headers is blank"); a confirm email has none.
        if !headers.is_empty() {
            body["headers"] = Value::Object(headers);
        }
        body
    }

    async fn call(&self, from: &Sender, e: &Email) -> Result<(), MailError> {
        let res = self
            .http
            .post(&self.url)
            .header("api-key", &self.key)
            .header("accept", "application/json")
            .json(&Self::body(from, e))
            .send()
            .await
            .map_err(|e| {
                // No connection: Brevo never saw it. Anything later (a read timeout) may have
                // been sent.
                if e.is_connect() {
                    MailError::Unavailable("unreachable".into())
                } else if e.is_builder() {
                    MailError::Misconfigured("the request couldn't be built".into())
                } else {
                    MailError::Uncertain(if e.is_timeout() { "timed out".into() } else { "no answer".into() })
                }
            })?;
        let status = res.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(());
        }
        let v: Value = res.json().await.unwrap_or(Value::Null);
        Err(classify(status, v["code"].as_str().unwrap_or(""), v["message"].as_str().unwrap_or("")))
    }
}

/// What Brevo's error answer means for the relay. `message` is kept only for our own setup
/// errors, with addresses blanked out (it can quote one); never for the recipient's.
pub(crate) fn classify(status: u16, code: &str, message: &str) -> MailError {
    // Brevo's `code` (`invalid_parameter`, `unauthorized`, …) is safe to keep.
    let code: String = code.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(40).collect();
    let why = format!("Brevo answered {status} {code}").trim().to_string();
    let msg = message.to_ascii_lowercase();
    let limit_words = ["credit", "limit", "quota", "exceed"];
    match status {
        400 if about_recipient(&code, &msg) => MailError::Rejected(why),
        402 | 429 => MailError::Limited(why),
        403 if code == "not_enough_credits" || limit_words.iter().any(|w| msg.contains(w)) => MailError::Limited(why),
        // Our own setup: Brevo's message names the field at fault ("sender is missing"), so
        // it's kept for the log, with any address blanked out.
        400..=499 => MailError::Misconfigured(with_said(why, message)),
        503 => MailError::Unavailable(why),
        _ => MailError::Uncertain(why),
    }
}

/// `why` plus Brevo's message with every word holding an `@` blanked, at most 160 characters.
fn with_said(why: String, message: &str) -> String {
    let said: String = message
        .split_whitespace()
        .map(|w| if w.contains('@') { "[address]" } else { w })
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .take(160)
        .collect();
    if said.is_empty() { why } else { format!("{why}: {said}") }
}

/// A 400 that is clearly about the recipient: a blocked contact, or a bad parameter naming
/// the `to` address. One about the sender is our setup, not the address.
fn about_recipient(code: &str, msg: &str) -> bool {
    if ["blacklist", "blocklist", "blocked", "unsubscribed contact"].iter().any(|w| msg.contains(w)) {
        return true;
    }
    if !matches!(code, "invalid_parameter" | "missing_parameter") || msg.contains("sender") {
        return false;
    }
    let words: Vec<&str> =
        msg.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).collect();
    // "email is not valid in to", "to[0].email is invalid", "recipient …"; not "unable to".
    msg.contains("recipient")
        || msg.contains("to[")
        || msg.contains("'to'")
        || msg.contains("\"to\"")
        || words.windows(2).any(|w| w == ["in", "to"] || w == ["to", "email"] || w == ["to", "is"])
        || words.last() == Some(&"to")
}

impl Mailer for BrevoMailer {
    fn send<'a>(&'a self, from: &'a Sender, email: &'a Email) -> SendFuture<'a> {
        Box::pin(self.call(from, email))
    }
}
