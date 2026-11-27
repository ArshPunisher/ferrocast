//! Stream polls in the engine: native platform polls (Twitch Helix, YouTube live chat)
//! when supported and `!vote N` chat polls elsewhere (or as fallback), live results to the
//! dashboard and poll overlay widgets, and chat result announcements on `!vote` platforms.

use std::collections::HashSet;
use std::time::Duration;

use ferrocast_connector::{ConnectorError, NativePollSpec, max_chat_chars, split_chat};
use ferrocast_core::command::safe_reply;
use ferrocast_core::event::{Event, EventKind, Origin, Platform};
use ferrocast_core::overlay::WidgetKind;
use ferrocast_core::poll::{
    self, PlatformPollCapability, PlatformPollMode, PlatformPollState, PlatformPollStatus, Poll, PollCapabilityMode,
    PollLimits, PollSpec, PollView, PollsCapabilities, VoteRefused,
};
use serde_json::json;

use crate::frames::{Audience, LiveFrame, Notice, NoticeLevel, OverlayFrame};
use crate::{Engine, EngineError, now};

/// Results are pushed at most this often while votes pour in.
const FLUSH: Duration = Duration::from_millis(250);
/// Backup native poll sync interval in case EventSub or streamList misses a vote beat.
const NATIVE_POLL_REFRESH: Duration = Duration::from_secs(5);

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

fn limit_reason(spec: &PollSpec, limits: &PollLimits) -> Option<&'static str> {
    if spec.question.chars().count() > limits.question_chars
        || spec.options.iter().any(|o| o.chars().count() > limits.option_chars)
    {
        return Some("text_too_long_for_native");
    }
    if spec.options.len() < limits.min_options || spec.options.len() > limits.max_options {
        return Some("options_count_unsupported_for_native");
    }
    if spec.duration_s.is_none() && !limits.supports_no_timer {
        return Some("no_timer_unsupported_for_native");
    }
    None
}

async fn watch_native_polls(engine: Engine, poll_id: String) {
    loop {
        tokio::time::sleep(NATIVE_POLL_REFRESH).await;
        let targets: Vec<(Platform, String)> = {
            let guard = engine.inner.poll.lock().unwrap_or_else(|p| p.into_inner());
            let Some(p) = guard.poll.as_ref() else { return };
            if p.id != poll_id || !p.is_open(now()) {
                return;
            }
            p.platforms
                .iter()
                .filter(|s| s.mode == PlatformPollMode::Native && s.state == PlatformPollState::Active)
                .filter_map(|s| s.external_id.clone().map(|id| (s.platform, id)))
                .collect()
        };
        if targets.is_empty() {
            return;
        }
        for (platform, ext_id) in targets {
            let Some(c) = engine.connector(platform) else { continue };
            if let Ok(Some(tally)) = c.get_native_poll(&ext_id).await {
                engine.on_native_poll_event(platform, &tally.external_id, &tally.counts, tally.ended).await;
            }
        }
    }
}

impl Engine {
    /// Reports per-platform native vs `!vote` capabilities and limits for connected chat platforms.
    pub async fn poll_capabilities(&self) -> PollsCapabilities {
        let mut platforms = Vec::new();
        for p in Platform::CHAT {
            let Some(c) = self.connector(p) else { continue };
            let cap = c.poll_capability().await.unwrap_or_else(|| PlatformPollCapability {
                platform: p,
                mode: PollCapabilityMode::ChatFallback,
                reason: Some("platform_unsupported".into()),
                limits: poll::DEFAULT_LIMITS,
            });
            platforms.push(cap);
        }
        PollsCapabilities { platforms }
    }

    /// The running poll, or the last one that finished (so its result stays on screen).
    pub fn poll(&self) -> Option<PollView> {
        self.inner.poll.lock().unwrap_or_else(|p| p.into_inner()).poll.as_ref().map(Poll::view)
    }

    /// Starts a poll using native APIs where supported and `!vote` elsewhere (when `chat_fallback`
    /// is enabled). 409 while another poll is still open.
    pub async fn start_poll(&self, spec: PollSpec) -> Result<PollView, EngineError> {
        let spec = spec.cleaned().map_err(EngineError::Invalid)?;
        let _op = self.inner.poll_op.lock().await;
        let at = now();
        if self.inner.poll.lock().unwrap_or_else(|p| p.into_inner()).poll.as_ref().is_some_and(|p| p.is_open(at)) {
            return Err(EngineError::Conflict("A poll is already running. End it before starting another.".into()));
        }

        let mut statuses = Vec::new();
        let mut initial_tallies: Vec<(Platform, Vec<u64>)> = Vec::new();
        let native_spec = NativePollSpec {
            question: spec.question.clone(),
            options: spec.options.clone(),
            duration_s: spec.duration_s,
        };

        for p in Platform::CHAT {
            let Some(c) = self.connector(p) else { continue };
            let cap = c.poll_capability().await.unwrap_or_else(|| PlatformPollCapability {
                platform: p,
                mode: PollCapabilityMode::ChatFallback,
                reason: Some("platform_unsupported".into()),
                limits: poll::DEFAULT_LIMITS,
            });
            if cap.mode == PollCapabilityMode::Native {
                if let Some(reason) = limit_reason(&spec, &cap.limits) {
                    if spec.chat_fallback {
                        statuses.push(PlatformPollStatus {
                            platform: p,
                            mode: PlatformPollMode::Chat,
                            external_id: None,
                            state: PlatformPollState::FailedFallback,
                            note: Some(reason.into()),
                        });
                    }
                    continue;
                }
                match c.create_native_poll(&native_spec).await {
                    Ok(tally) => {
                        initial_tallies.push((p, tally.counts));
                        statuses.push(PlatformPollStatus {
                            platform: p,
                            mode: PlatformPollMode::Native,
                            external_id: Some(tally.external_id),
                            state: if tally.ended { PlatformPollState::Ended } else { PlatformPollState::Active },
                            note: None,
                        });
                    }
                    Err(err) => {
                        tracing::warn!(platform = %p, error = %err, "native poll failed; falling back if enabled");
                        if spec.chat_fallback {
                            let note = match &err {
                                ConnectorError::MissingScope { .. } => "needs_scope".to_string(),
                                _ => err.to_string(),
                            };
                            statuses.push(PlatformPollStatus {
                                platform: p,
                                mode: PlatformPollMode::Chat,
                                external_id: None,
                                state: PlatformPollState::FailedFallback,
                                note: Some(note),
                            });
                        }
                    }
                }
            } else if spec.chat_fallback {
                statuses.push(PlatformPollStatus {
                    platform: p,
                    mode: PlatformPollMode::Chat,
                    external_id: None,
                    state: PlatformPollState::Active,
                    note: cap.reason,
                });
            }
        }

        if !spec.chat_fallback && statuses.is_empty() {
            return Err(EngineError::Invalid(
                "No connected platform supports native polls right now. Enable chat (!vote) fallback or reconnect Twitch."
                    .into(),
            ));
        }

        let poll = {
            let mut slot = self.inner.poll.lock().unwrap_or_else(|p| p.into_inner());
            if slot.poll.as_ref().is_some_and(|p| p.is_open(at)) {
                return Err(EngineError::Conflict("A poll is already running. End it before starting another.".into()));
            }
            let mut p = Poll::start(ferrocast_store::new_id(), spec, at);
            p.set_platforms(statuses);
            for (platform, counts) in initial_tallies {
                p.set_native_tally(platform, &counts);
            }
            slot.poll = Some(p.clone());
            slot.dirty = false;
            p
        };
        let view = poll.view();
        self.broadcast_poll(Some(view.clone()));
        if poll.platforms.is_empty() {
            self.announce(&poll.announcement(), "post the poll");
        } else if let Some(chat_set) = poll.chat_platforms()
            && !chat_set.is_empty()
        {
            self.announce_to(&poll.announcement(), "post the poll", &chat_set);
        }
        // Closes it on time when timed, unless it was ended or replaced first.
        if let Some(ends_at) = poll.ends_at {
            let engine = self.clone();
            let id = poll.id.clone();
            tokio::spawn(async move {
                let wait = u64::try_from((ends_at.0 - now().0).max(0)).unwrap_or(0);
                tokio::time::sleep(Duration::from_millis(wait)).await;
                let _ = engine.finish_poll(Some(&id)).await;
            });
        }
        if poll.platforms.iter().any(|p| p.mode == PlatformPollMode::Native && p.state == PlatformPollState::Active) {
            tokio::spawn(watch_native_polls(self.clone(), poll.id.clone()));
        }
        Ok(view)
    }

    /// Ends the poll now and posts the result. 404 without a poll; 409 when it already
    /// ended (two quick clicks, or the timer got there first).
    pub async fn end_poll(&self) -> Result<PollView, EngineError> {
        self.finish_poll(None).await
    }

    /// Closes the poll (only `id`, when given) once: whoever gets here first terminates active
    /// native platform polls and posts the result on chat-fallback platforms.
    pub(crate) async fn finish_poll(&self, id: Option<&str>) -> Result<PollView, EngineError> {
        let (poll_id, to_end) = {
            let mut guard = self.inner.poll.lock().unwrap_or_else(|p| p.into_inner());
            let PollSlot { poll, dirty } = &mut *guard;
            let Some(p) = poll.as_mut() else { return Err(NO_POLL) };
            if id.is_some_and(|id| id != p.id) {
                return Err(NO_POLL);
            }
            let to_end: Vec<(Platform, String)> = p
                .platforms
                .iter()
                .filter(|s| s.mode == PlatformPollMode::Native && s.state == PlatformPollState::Active)
                .filter_map(|s| s.external_id.clone().map(|ext| (s.platform, ext)))
                .collect();
            if !p.close() {
                return Err(EngineError::Conflict("That poll already ended.".into()));
            }
            *dirty = false;
            (p.id.clone(), to_end)
        };
        for (platform, ext_id) in to_end {
            let Some(c) = self.connector(platform) else { continue };
            match c.end_native_poll(&ext_id).await {
                Ok(Some(tally)) => {
                    let mut guard = self.inner.poll.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(p) = guard.poll.as_mut()
                        && p.id == poll_id
                    {
                        p.set_native_tally(platform, &tally.counts);
                    }
                }
                Ok(None) => {}
                Err(err) => tracing::warn!(%platform, error = %err, "couldn't end native poll on platform"),
            }
        }
        let poll = {
            let guard = self.inner.poll.lock().unwrap_or_else(|p| p.into_inner());
            match guard.poll.as_ref().filter(|p| p.id == poll_id) {
                Some(p) => p.clone(),
                None => return Err(NO_POLL),
            }
        };
        let view = poll.view();
        self.broadcast_poll(Some(view.clone()));
        if poll.platforms.is_empty() {
            self.announce(&poll.result(), "post the poll result");
        } else if let Some(chat_set) = poll.chat_platforms()
            && !chat_set.is_empty()
        {
            self.announce_to(&poll.result(), "post the poll result", &chat_set);
        }
        Ok(view)
    }

    /// Removes the poll without posting anything. 404 when there's none.
    pub fn clear_poll(&self) -> Result<(), EngineError> {
        let to_end: Vec<(Platform, String)> = {
            let mut slot = self.inner.poll.lock().unwrap_or_else(|p| p.into_inner());
            let Some(p) = slot.poll.take() else {
                return Err(NO_POLL);
            };
            slot.dirty = false;
            if p.closed {
                Vec::new()
            } else {
                p.platforms
                    .into_iter()
                    .filter(|s| s.mode == PlatformPollMode::Native && s.state == PlatformPollState::Active)
                    .filter_map(|s| s.external_id.map(|ext| (s.platform, ext)))
                    .collect()
            }
        };
        for (platform, ext_id) in to_end {
            if let Some(c) = self.connector(platform) {
                tokio::spawn(async move {
                    let _ = c.end_native_poll(&ext_id).await;
                });
            }
        }
        self.broadcast_poll(None);
        Ok(())
    }

    /// Applies an incoming tally or end update from a platform-native poll.
    pub(crate) async fn on_native_poll_event(
        &self,
        platform: Platform,
        external_id: &str,
        counts: &[u64],
        ended: bool,
    ) {
        let (changed, ended_here, should_finish, poll_id) = {
            let mut guard = self.inner.poll.lock().unwrap_or_else(|p| p.into_inner());
            let PollSlot { poll, dirty } = &mut *guard;
            let Some(p) = poll.as_mut() else { return };
            if !p.is_open(now()) {
                return;
            }
            let matches_platform = p.platforms.iter().any(|s| {
                s.platform == platform
                    && s.mode == PlatformPollMode::Native
                    && s.external_id.as_deref() == Some(external_id)
            });
            if !matches_platform {
                return;
            }
            let mut changed = p.set_native_tally(platform, counts);
            let ended_here = ended && p.set_platform_state(platform, PlatformPollState::Ended, None);
            changed |= ended_here;
            let should_finish = p.all_platforms_ended();
            if changed && !should_finish && !ended_here {
                *dirty = true;
            }
            (changed, ended_here, should_finish, p.id.clone())
        };
        if should_finish {
            let _ = self.finish_poll(Some(&poll_id)).await;
        } else if ended_here && changed {
            let view = {
                let mut guard = self.inner.poll.lock().unwrap_or_else(|p| p.into_inner());
                guard.dirty = false;
                guard.poll.as_ref().map(Poll::view)
            };
            self.broadcast_poll(view);
        }
    }

    /// Counts `!vote N` from live chat on platforms running in `!vote` mode. `voter` is the
    /// person (linked accounts share one), so switching platforms doesn't give a second vote.
    pub(crate) fn poll_vote(&self, e: &Event, voter: &str) -> Option<Result<poll::Voted, VoteRefused>> {
        let EventKind::ChatMessage { content, .. } = &e.kind else { return None };
        if e.origin != Origin::Live || e.channel.is_some() {
            return None;
        }
        let choice = poll::parse_vote(&content.text)?;
        let mut guard = self.inner.poll.lock().unwrap_or_else(|p| p.into_inner());
        let PollSlot { poll, dirty } = &mut *guard;
        let p = poll.as_mut()?;
        if !p.allows_chat_vote(e.platform) {
            return None;
        }
        let r = p.vote(voter, choice, now());
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
        let targets: HashSet<Platform> = Platform::CHAT.into_iter().collect();
        self.announce_to(text, what, &targets);
    }

    pub(crate) fn announce_to(&self, text: &str, what: &'static str, targets: &HashSet<Platform>) {
        let text = safe_reply(text);
        if text.is_empty() {
            return;
        }
        let mut posted = false;
        for p in Platform::CHAT {
            if !targets.contains(&p) || self.connector(p).is_none() {
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
                &Notice::new(NoticeLevel::Warn, "No chat is connected")
                    .key("no-chat")
                    .detail(format!("Ferrocast couldn't {what}. Connect a platform in Settings, Accounts.")),
            ));
        }
    }
}
