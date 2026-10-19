//! A mail sender for tests: keeps every email it was asked to send (so a test can open the
//! confirm link in it), and can be told to fail: always ([`FakeMailer::fail_with`]) or for
//! the next few sends in order ([`FakeMailer::script`]).

use std::collections::VecDeque;
use std::sync::Mutex;

use super::mailer::{Email, MailError, Mailer, SendFuture, Sender};

#[derive(Debug, Default)]
pub struct FakeMailer {
    sent: Mutex<Vec<Email>>,
    fail: Mutex<Option<MailError>>,
    next: Mutex<VecDeque<Result<(), MailError>>>,
    tried: Mutex<usize>,
    hook: Mutex<Hook>,
}

/// Runs inside each send, before it answers (tests: something changes mid-run).
#[derive(Default)]
struct Hook(Option<Box<dyn Fn() + Send + Sync>>);

impl std::fmt::Debug for Hook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() { "Hook(set)" } else { "Hook(none)" })
    }
}

impl FakeMailer {
    pub fn new() -> FakeMailer {
        FakeMailer::default()
    }

    /// Every email sent, oldest first (failed ones aren't kept).
    pub fn sent(&self) -> Vec<Email> {
        self.sent.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The newest email, if any.
    pub fn last(&self) -> Option<Email> {
        self.sent().pop()
    }

    /// How many sends were asked for, failed ones included.
    pub fn tried(&self) -> usize {
        *self.tried.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Fail every send with `e` from now on (`None`: work again).
    pub fn fail_with(&self, e: Option<MailError>) {
        *self.fail.lock().unwrap_or_else(|p| p.into_inner()) = e;
    }

    /// Runs `f` inside every send from now on.
    pub fn during_send(&self, f: impl Fn() + Send + Sync + 'static) {
        self.hook.lock().unwrap_or_else(|p| p.into_inner()).0 = Some(Box::new(f));
    }

    /// The next sends answer these, in order (before `fail_with` applies).
    pub fn script(&self, answers: Vec<Result<(), MailError>>) {
        self.next.lock().unwrap_or_else(|p| p.into_inner()).extend(answers);
    }
}

impl Mailer for FakeMailer {
    fn send<'a>(&'a self, _from: &'a Sender, email: &'a Email) -> SendFuture<'a> {
        Box::pin(async move {
            *self.tried.lock().unwrap_or_else(|p| p.into_inner()) += 1;
            if let Some(f) = &self.hook.lock().unwrap_or_else(|p| p.into_inner()).0 {
                f();
            }
            let scripted = self.next.lock().unwrap_or_else(|p| p.into_inner()).pop_front();
            let answer = match scripted {
                Some(a) => a,
                None => match self.fail.lock().unwrap_or_else(|p| p.into_inner()).clone() {
                    Some(e) => Err(e),
                    None => Ok(()),
                },
            };
            if answer.is_ok() {
                self.sent.lock().unwrap_or_else(|p| p.into_inner()).push(email.clone());
            }
            answer
        })
    }
}
