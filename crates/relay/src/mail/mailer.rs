//! What a mail sender is: one call that sends a transactional email. `BrevoMailer` in
//! production, `FakeMailer` in tests. The address is personal data: an [`Email`]'s `Debug`
//! never shows it, and no error carries it.

use std::future::Future;
use std::pin::Pin;

/// Who the mail is from (`MAIL_FROM`, default `Ferrocast <reports@mail.clearcraftlabs.com>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sender {
    pub name: String,
    pub email: String,
}

pub const DEFAULT_FROM: &str = "Ferrocast <reports@mail.clearcraftlabs.com>";

impl Sender {
    /// `Name <address>` or a bare address; `None` when it isn't an address.
    pub fn parse(raw: &str) -> Option<Sender> {
        let raw = raw.trim();
        let (name, email) = match (raw.rfind('<'), raw.ends_with('>')) {
            (Some(i), true) => (raw[..i].trim().trim_matches('"').trim(), raw[i + 1..raw.len() - 1].trim()),
            _ => ("", raw),
        };
        if !crate::billing::valid_email(email) || name.chars().any(char::is_control) {
            return None;
        }
        Some(Sender { name: if name.is_empty() { "Ferrocast".into() } else { name.into() }, email: email.into() })
    }
}

/// One email. `tag` names the kind (`confirm`, `weekly`) for the provider's statistics.
#[derive(Clone, PartialEq, Eq)]
pub struct Email {
    pub to: String,
    pub subject: String,
    pub html: String,
    pub text: String,
    /// Extra headers: `List-Unsubscribe`, `List-Unsubscribe-Post`.
    pub headers: Vec<(String, String)>,
    pub tag: &'static str,
}

impl std::fmt::Debug for Email {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Email")
            .field("to", &"<redacted>")
            .field("tag", &self.tag)
            .field("html_len", &self.html.len())
            .finish_non_exhaustive()
    }
}

/// Why a mail couldn't go. Never holds the address.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MailError {
    /// The provider refused this message because of its recipient (Brevo 400 naming the `to`
    /// address, or a blocked / blacklisted contact). Sending it again won't help: the address
    /// is paused.
    #[error("the mail provider refused the recipient: {0}")]
    Rejected(String),
    /// Our side is wrong (key refused, sender not allowed, a request Brevo can't read): nothing
    /// a streamer can fix; the run stops, the claim is given back, logged for the owner.
    #[error("the mail provider refused the relay's account: {0}")]
    Misconfigured(String),
    /// The provider's daily limit or credits ran out (402, 403 about a limit, 429): nothing
    /// more goes today; the claim is given back for tomorrow.
    #[error("the mail provider's sending limit is reached: {0}")]
    Limited(String),
    /// The request wasn't accepted (no connection, 503): safe to try again later.
    #[error("the mail provider is unavailable: {0}")]
    Unavailable(String),
    /// The request went but no clear answer came back (read timeout, 500/502/504): it may
    /// have been sent, so it counts as sent (never twice).
    #[error("the mail provider's answer was lost: {0}")]
    Uncertain(String),
}

pub type SendFuture<'a> = Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>>;

/// A mail sender. Boxed futures so the relay can hold any of them as `dyn`.
pub trait Mailer: Send + Sync + std::fmt::Debug {
    fn send<'a>(&'a self, from: &'a Sender, email: &'a Email) -> SendFuture<'a>;
}
