//! Chat polls in the engine: start from the dashboard, `!vote N` from any platform, live
//! results to the dashboard and to overlays with a poll widget, and the result posted to
//! chat at the end.

use std::time::Duration;

use ferrocast_connector::{max_chat_chars, split_chat};
use ferrocast_core::command::safe_reply;
use ferrocast_core::event::{Event, EventKind, Origin, Platform};
use ferrocast_core::overlay::WidgetKind;
use ferrocast_core::poll::{self, Poll, PollSpec, PollView, VoteRefused};
use serde_json::json;

use crate::frames::{Audience, LiveFrame, Notice, NoticeLevel, OverlayFrame};
use crate::{Engine, EngineError, now};

/// Results are pushed at most this often while votes pour in.
const FLUSH: Duration = Duration::from_millis(250);

/// The current (or last finished) poll, and whether its numbers changed since the last push.
#[derive(Debug, Default)]
pub(crate) struct PollSlot {
    pub(crate) poll: Option<Poll>,
    pub(crate) dirty: bool,
}

const NO_POLL: EngineError = EngineError::Missing {
    code: "poll_not_found",
    message: "There's no poll running.",
    hint: "Start one from Community → Polls.",
};

pub(crate) async fn run(engine: Engine) {
    loop {
        tokio::time::sleep(FLUSH).await;
        let view = {
            let mut slot = engine.inner.poll.lock().unwrap_or_else(|p| p.into_inner());
            if !std::mem::take(&mut slot.dirty) {
                continue;
            }
            slot.poll.as_ref().map(Poll::view)
        };
        engine.broadcast_poll(view);
    }
}

impl Engine {
    /// The running poll, or the last one that finished (so its result stays on screen).
    pub fn poll(&self) -> Option<PollView> {
        self.inner.poll.lock().unwrap_or_else(|p| p.into_inner()).poll.as_ref().map(Poll::view)
    }

    /// Starts a poll and announces it in chat on every connected platform. 409 while
    /// another poll is still open.
    pub async fn start_poll(&self, spec: PollSpec) -> Result<PollView, EngineError> {
        let spec = spec.cleaned().map_err(EngineError::Invalid)?;
        let at = now();
        let poll = {
            let mut slot = self.inner.poll.lock().unwrap_or_else(|p| p.into_inner());
            if slot.poll.as_ref().is_some_and(|p| p.is_open(at)) {
                return Err(EngineError::Conflict("A poll is already running. End it before starting another.".into()));
            }
            let p = Poll::start(ferrocast_store::new_id(), spec, at);
            slot.poll = Some(p.clone());
            slot.dirty = false;
            p
        };
        let view = poll.view();
        self.broadcast_poll(Some(view.clone()));
        self.announce(&poll.announcement(), "post the poll");
        // Closes it on time, unless it was ended or replaced first.
        let engine = self.clone();
        let (id, ends_at) = (poll.id.clone(), poll.ends_at);
        tokio::spawn(async move {
            let wait = u64::try_from((ends_at.0 - now().0).max(0)).unwrap_or(0);
            tokio::time::sleep(Duration::from_millis(wait)).await;
            let _ = engine.finish_poll(Some(&id)).await;
        });
        Ok(view)
    }

    /// Ends the poll now and posts the result. 404 without a poll; 409 when it already
    /// ended (two quick clicks, or the timer got there first).
    pub async fn end_poll(&self) -> Result<PollView, EngineError> {
        self.finish_poll(None).await
    }

    /// Closes the poll (only `id`, when given) once: whoever gets here first posts the
    /// result, everyone after gets a conflict.
    pub(crate) async fn finish_poll(&self, id: Option<&str>) -> Result<PollView, EngineError> {
        let poll = {
            let mut guard = self.inner.poll.lock().unwrap_or_else(|p| p.into_inner());
            let PollSlot { poll, dirty } = &mut *guard;
            let Some(p) = poll.as_mut() else { return Err(NO_POLL) };
            if id.is_some_and(|id| id != p.id) {
                return Err(NO_POLL);
            }
            if !p.close() {
                return Err(EngineError::Conflict("That poll already ended.".into()));
            }
            *dirty = false;
            p.clone()
        };
        let view = poll.view();
        self.broadcast_poll(Some(view.clone()));
        self.announce(&poll.result(), "post the poll result");
        Ok(view)
    }

    /// Removes the poll without posting anything. 404 when there's none.
    pub fn clear_poll(&self) -> Result<(), EngineError> {
        {
            let mut slot = self.inner.poll.lock().unwrap_or_else(|p| p.into_inner());
            if slot.poll.take().is_none() {
                return Err(NO_POLL);
            }
            slot.dirty = false;
        }
        self.broadcast_poll(None);
        Ok(())
    }

    /// Counts `!vote N` from live chat on this channel. `voter` is the person (linked
    /// accounts share one), so switching platforms doesn't give a second vote.
    pub(crate) fn poll_vote(&self, e: &Event, voter: &str) -> Option<Result<poll::Voted, VoteRefused>> {
        let EventKind::ChatMessage { content, .. } = &e.kind else { return None };
        if e.origin != Origin::Live || e.channel.is_some() {
            return None;
        }
        let choice = poll::parse_vote(&content.text)?;
        let mut guard = self.inner.poll.lock().unwrap_or_else(|p| p.into_inner());
        let PollSlot { poll, dirty } = &mut *guard;
        let r = poll.as_mut()?.vote(voter, choice, now());
        if matches!(r, Ok(poll::Voted::New | poll::Voted::Changed)) {
            *dirty = true;
        }
        Some(r)
    }

    pub(crate) fn broadcast_poll(&self, view: Option<PollView>) {
        self.send_live(LiveFrame::new("poll", &json!({ "poll": view })));
        self.send_overlay(OverlayFrame::new(
            Audience::Widget(WidgetKind::Poll),
            self.next_seq(),
            "state",
            &json!({ "poll": view }),
        ));
    }

    /// Posts a message on every connected chat platform, in the background, split to each
    /// platform's length limit. Failures become a notice; so does having no chat at all.
    pub(crate) fn announce(&self, text: &str, what: &'static str) {
        let text = safe_reply(text);
        if text.is_empty() {
            return;
        }
        let mut posted = false;
        for p in Platform::CHAT {
            if self.connector(p).is_none() {
                continue;
            }
            posted = true;
            let chunks = split_chat(&text, max_chat_chars(p));
            self.spawn_action(p, what, move |c| {
                Box::pin(async move {
                    for chunk in chunks {
                        c.send_chat(&chunk, None).await?;
                    }
                    Ok(())
                })
            });
        }
        if !posted {
            self.send_live(LiveFrame::new(
                "notice",
                &Notice {
                    key: Some("no-chat".into()),
                    level: NoticeLevel::Warn,
                    message: format!("No chat is connected, so Ferrocast couldn't {what}."),
                    action: None,
                },
            ));
        }
    }
}
