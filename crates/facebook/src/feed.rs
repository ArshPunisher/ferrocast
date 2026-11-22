//! Live comments → chat events: the Page's live video is polled through the Graph API
//! (comments have no push; this is the documented way, and the only way used).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use ferrocast_core::event::{Actor, Content, Event, EventKind, Millis, Platform, Roles};
use tokio::sync::mpsc::Sender;

use crate::graph::{Comment, Graph};

/// How often the comments are asked for while a live video is on.
pub const POLL_EVERY: Duration = Duration::from_secs(4);
/// How often the Page is asked whether it is live, when it isn't.
pub const IDLE_EVERY: Duration = Duration::from_secs(20);

/// A comment as a chat message.
pub fn chat_event(c: &Comment) -> Event {
    let actor = Actor {
        platform: Platform::Facebook,
        id: c.from_id.clone(),
        login: None,
        display_name: c.from_name.clone(),
        avatar_url: c.avatar_url.clone(),
        color: None,
        roles: Roles::empty(),
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

/// Runs until the sink closes: finds the Page's live video, reads its comments as they come.
/// `live_hint` is a video the engine knows is live (just made by Go live), tried first.
pub async fn run(graph: Arc<Graph>, sink: Sender<Event>, live_hint: Option<String>) {
    let mut hint = live_hint;
    let mut seen: HashSet<String> = HashSet::new();
    loop {
        let live = match hint.take() {
            Some(id) => Some(id),
            None => match graph.current_live().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "Facebook: couldn't ask whether the Page is live");
                    None
                }
            },
        };
        let Some(live_id) = live else {
            tokio::time::sleep(IDLE_EVERY).await;
            continue;
        };
        let mut cursor: Option<String> = None;
        loop {
            match graph.comments(&live_id, cursor.as_deref()).await {
                Ok((list, next)) => {
                    for c in list {
                        if seen.insert(c.id.clone()) && sink.send(chat_event(&c)).await.is_err() {
                            return;
                        }
                    }
                    if next.is_some() {
                        cursor = next;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Facebook: couldn't read the comments");
                    // A video that ended, or a sign-in that lapsed: look again from the top.
                    break;
                }
            }
            if sink.is_closed() {
                return;
            }
            tokio::time::sleep(POLL_EVERY).await;
        }
        tokio::time::sleep(IDLE_EVERY).await;
    }
}
