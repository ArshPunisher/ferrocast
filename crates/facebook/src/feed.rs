//! Live comments → chat events: the Page's live video is polled through the Graph API
//! (comments have no push; this is the documented way, and the only way used).

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use ferrocast_core::event::{Actor, ConnectorStatus, Content, Event, EventKind, Millis, Platform, Roles};
use tokio::sync::mpsc::Sender;

use crate::FacebookError;
use crate::graph::{Comment, Graph};

/// How often the comments are asked for while a live video is on.
pub const POLL_EVERY: Duration = Duration::from_secs(4);
/// How often the Page is asked whether it is live, when it isn't.
pub const IDLE_EVERY: Duration = Duration::from_secs(20);
/// How often a live video is checked to still be the Page's live one: an ended video keeps
/// answering its comments (a recording has them), so only this notices it is over.
pub const RECHECK_EVERY: Duration = Duration::from_secs(60);
/// The longest wait Facebook's "slow down" is obeyed for.
pub const RATE_WAIT_MAX: Duration = Duration::from_secs(15 * 60);
/// Comment ids remembered per live video (so a repeat is not sent twice), at most.
const SEEN_CAP: usize = 2_000;

/// The waits of [`run`], so a test can use short ones.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub poll: Duration,
    pub idle: Duration,
    pub recheck: Duration,
    /// The most a rate-limit answer is waited out for.
    pub rate_cap: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing { poll: POLL_EVERY, idle: IDLE_EVERY, recheck: RECHECK_EVERY, rate_cap: RATE_WAIT_MAX }
    }
}

/// The comment ids already sent for this live video, oldest forgotten first once there are
/// `cap` of them (it would grow for the whole length of a long stream otherwise).
#[derive(Debug)]
pub struct SeenIds {
    set: HashSet<String>,
    order: VecDeque<String>,
    cap: usize,
}

impl SeenIds {
    pub fn new(cap: usize) -> Self {
        SeenIds { set: HashSet::new(), order: VecDeque::new(), cap: cap.max(1) }
    }

    /// True when the id was not there (it is now).
    pub fn insert(&mut self, id: String) -> bool {
        if self.set.contains(&id) {
            return false;
        }
        self.set.insert(id.clone());
        self.order.push_back(id);
        while self.order.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }

    pub fn len(&self) -> usize {
        self.set.len()
    }

    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    pub fn clear(&mut self) {
        self.set.clear();
        self.order.clear();
    }
}

/// A comment as a chat message.
pub fn chat_event(c: &Comment) -> Event {
    chat_event_for(c, "")
}

/// A comment as a chat message on the Page `page_id`: one the Page itself wrote (the streamer, or
/// the bot) carries the broadcaster role, so filters, the welcome and commands treat it as theirs.
pub fn chat_event_for(c: &Comment, page_id: &str) -> Event {
    let own = !page_id.is_empty() && c.from_id.as_deref() == Some(page_id);
    let actor = Actor {
        platform: Platform::Facebook,
        id: c.from_id.clone(),
        login: None,
        display_name: c.from_name.clone(),
        avatar_url: c.avatar_url.clone(),
        color: None,
        roles: if own { Roles::BROADCASTER } else { Roles::empty() },
        badges: Vec::new(),
        anonymous: false,
    };
    Event::live(
        Platform::Facebook,
        Some(c.id.clone()),
        Millis(c.created_ms),
        EventKind::ChatMessage { actor, content: Content::plain(&c.message), reply_to: None, is_action: false },
    )
}

async fn sign_in_needed(sink: &Sender<Event>) {
    let _ = sink
        .send(Event::live(
            Platform::Facebook,
            None,
            Millis(crate::now_ms()),
            EventKind::Connector(ConnectorStatus::SignInNeeded),
        ))
        .await;
}

/// Runs until the sink closes or the sign-in lapses: finds the Page's live video, reads its
/// comments as they come. `live_hint` is a video the engine knows is live (just made by Go
/// live), tried first.
pub async fn run(graph: Arc<Graph>, sink: Sender<Event>, live_hint: Option<String>) {
    run_with(graph, sink, live_hint, Timing::default()).await;
}

/// [`run`] with its waits given.
pub async fn run_with(graph: Arc<Graph>, sink: Sender<Event>, live_hint: Option<String>, t: Timing) {
    let mut hint = live_hint;
    let mut seen = SeenIds::new(SEEN_CAP);
    let mut last_live: Option<String> = None;
    loop {
        let live = match hint.take() {
            Some(id) => Some(id),
            None => match graph.current_live().await {
                Ok(v) => v,
                // A lapsed or revoked sign-in is said once (the dashboard offers Reconnect), and the
                // feed stops: asking again every few seconds would only be refused again.
                Err(FacebookError::SignInNeeded) => {
                    sign_in_needed(&sink).await;
                    return;
                }
                Err(FacebookError::RateLimited(d)) => {
                    tokio::time::sleep(d.min(t.rate_cap)).await;
                    continue;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Facebook: couldn't ask whether the Page is live");
                    None
                }
            },
        };
        let Some(live_id) = live else {
            tokio::time::sleep(t.idle).await;
            continue;
        };
        if last_live.as_deref() != Some(live_id.as_str()) {
            seen.clear();
            last_live = Some(live_id.clone());
        }
        let mut cursor: Option<String> = None;
        let mut checked = std::time::Instant::now();
        loop {
            match graph.comments(&live_id, cursor.as_deref()).await {
                Ok((list, next)) => {
                    for c in list {
                        if seen.insert(c.id.clone()) && sink.send(chat_event_for(&c, graph.page_id())).await.is_err() {
                            return;
                        }
                    }
                    if next.is_some() {
                        cursor = next;
                    }
                }
                Err(FacebookError::SignInNeeded) => {
                    sign_in_needed(&sink).await;
                    return;
                }
                Err(FacebookError::RateLimited(d)) => {
                    // Stay on this video and wait what Facebook asked for.
                    tokio::time::sleep(d.min(t.rate_cap)).await;
                    continue;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Facebook: couldn't read the comments");
                    // A video that ended: look again from the top.
                    break;
                }
            }
            if sink.is_closed() {
                return;
            }
            // Now and then: is this still the Page's live video? An ended one keeps answering.
            if checked.elapsed() >= t.recheck {
                checked = std::time::Instant::now();
                match graph.current_live().await {
                    Ok(Some(id)) if id == live_id => {}
                    Ok(_) => break,
                    Err(FacebookError::SignInNeeded) => {
                        sign_in_needed(&sink).await;
                        return;
                    }
                    Err(_) => {}
                }
            }
            tokio::time::sleep(t.poll).await;
        }
        tokio::time::sleep(t.idle).await;
    }
}
