//! Giveaways in the engine: keyword entry from any platform (optionally paid with points),
//! one entry per person, a winner drawn with the OS's randomness, redraws, and the winner
//! announced in chat. Entry fees are refunded if the giveaway is cancelled or the app
//! stops before it finished.

use std::collections::HashSet;
use std::time::Duration;

use ferrocast_core::command::safe_reply;
use ferrocast_core::event::{Event, EventKind, Origin};
use ferrocast_core::giveaway::{Giveaway, GiveawayCard, GiveawaySpec, GiveawayView};
use ferrocast_core::loyalty;
use ferrocast_core::overlay::WidgetKind;
use ferrocast_store::repo;
use ferrocast_store::repo::loyalty::Change;
use serde_json::json;

use crate::frames::{Audience, LiveFrame, OverlayFrame};
use crate::{Engine, EngineError, now};

const FLUSH: Duration = Duration::from_millis(250);

#[derive(Debug, Default)]
pub(crate) struct GiveawaySlot {
    pub(crate) giveaway: Option<Giveaway>,
    /// People already told they can't afford to enter (told once, not on every try).
    told: HashSet<String>,
    dirty: bool,
}

const NO_GIVEAWAY: EngineError = EngineError::Missing {
    code: "giveaway_not_found",
    message: "There's no giveaway running.",
    hint: "Start one from Community → Giveaway.",
};

fn os_random() -> Result<u64, EngineError> {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).map_err(|e| {
        EngineError::Unavailable(format!(
            "This computer's random number source isn't working ({e}), so no winner was drawn."
        ))
    })?;
    Ok(u64::from_le_bytes(b))
}

/// Pushes entry counts at most every 250 ms, and refunds fees left over from a run that
/// stopped mid-giveaway.
pub(crate) async fn run(engine: Engine) {
    match engine.db(|s| repo::giveaways::refund(s, None)).await {
        Ok(n) if n > 0 => tracing::info!(refunded = n, "refunded giveaway entries from an unfinished giveaway"),
        Ok(_) => {}
        Err(err) => tracing::warn!(%err, "couldn't refund unfinished giveaway entries"),
    }
    loop {
        tokio::time::sleep(FLUSH).await;
        let flush = {
            let mut slot = engine.inner.giveaway.lock().await;
            let dirty = std::mem::take(&mut slot.dirty);
            dirty.then(|| slot.giveaway.as_ref().map(|g| (g.view(), g.card())))
        };
        if let Some(v) = flush {
            engine.broadcast_giveaway(v);
        }
    }
}

impl Engine {
    pub async fn giveaway(&self) -> Option<GiveawayView> {
        self.inner.giveaway.lock().await.giveaway.as_ref().map(Giveaway::view)
    }

    pub(crate) async fn giveaway_card(&self) -> Option<GiveawayCard> {
        self.inner.giveaway.lock().await.giveaway.as_ref().map(Giveaway::card)
    }

    /// Starts taking entries and announces it. 409 while another giveaway hasn't ended.
    pub async fn start_giveaway(&self, spec: GiveawaySpec) -> Result<GiveawayView, EngineError> {
        let spec = spec.cleaned().map_err(EngineError::Invalid)?;
        if spec.cost > 0 && !self.loyalty().enabled {
            return Err(EngineError::Invalid(
                "cost: points are turned off, so entries can't cost points. Turn points on or make it free.".into(),
            ));
        }
        let g = {
            let mut slot = self.inner.giveaway.lock().await;
            if slot.giveaway.as_ref().is_some_and(|g| !g.ended) {
                return Err(EngineError::Conflict(
                    "A giveaway is already running. End it before starting another.".into(),
                ));
            }
            let g = Giveaway::start(ferrocast_store::new_id(), spec, now());
            slot.giveaway = Some(g.clone());
            slot.told.clear();
            slot.dirty = false;
            g
        };
        self.broadcast_giveaway(Some((g.view(), g.card())));
        self.announce(&g.announcement(), "post the giveaway");
        Ok(g.view())
    }

    /// Someone typed the keyword in live chat. `person` is shared by linked accounts.
    pub(crate) async fn giveaway_entry(&self, e: &Event, person: &str) {
        let EventKind::ChatMessage { actor, content, .. } = &e.kind else { return };
        if e.origin != Origin::Live || e.channel.is_some() {
            return;
        }
        // Held across the payment, so one person typing !join twice can't pay twice.
        let mut slot = self.inner.giveaway.lock().await;
        let Some(g) = slot.giveaway.as_ref() else { return };
        if !g.is_keyword(&content.text) || g.may_enter(person, actor).is_err() {
            return;
        }
        let (cost, id, name) = (g.cost, g.id.clone(), self.loyalty().name);
        let Some(uid) = actor.id.clone() else { return };
        if cost > 0 {
            let (p, gid, u) = (e.platform, id.clone(), uid.clone());
            match self.db(move |s| repo::giveaways::pay_entry(s, &gid, p, &u, cost)).await {
                Ok(Change::Done(_)) => {}
                Ok(Change::NotEnough { have }) => {
                    if slot.told.insert(person.to_string()) {
                        let reply = safe_reply(&format!(
                            "@{}, entering costs {} and you have {}.",
                            actor.display_name,
                            loyalty::label(cost, &name),
                            loyalty::label(have, &name)
                        ));
                        let reply_to = e.source_id.clone();
                        self.spawn_action(e.platform, "answer a giveaway entry", move |c| {
                            Box::pin(async move { c.send_chat(&reply, reply_to.as_deref()).await })
                        });
                    }
                    return;
                }
                Ok(Change::TooMuch { .. }) | Err(_) => return,
            }
        }
        let paid = cost;
        if let Some(g) = slot.giveaway.as_mut()
            && g.id == id
            && g.enter(person, actor, paid).is_ok()
        {
            slot.dirty = true;
        }
    }

    /// Stops taking entries. 409 when entries are already closed.
    pub async fn close_giveaway(&self) -> Result<GiveawayView, EngineError> {
        let (view, card) = {
            let mut slot = self.inner.giveaway.lock().await;
            let g = slot.giveaway.as_mut().ok_or(NO_GIVEAWAY)?;
            if g.ended || !g.open {
                return Err(EngineError::Conflict("Entries are already closed.".into()));
            }
            g.open = false;
            slot.dirty = false;
            let g = slot.giveaway.as_ref().ok_or(NO_GIVEAWAY)?;
            (g.view(), g.card())
        };
        self.broadcast_giveaway(Some((view.clone(), card)));
        Ok(view)
    }

    /// Draws the first winner (`redraw = false`) or another one. Two clicks at once can't
    /// both draw: the first wins, the second gets a 409.
    pub async fn draw_giveaway(&self, redraw: bool) -> Result<GiveawayView, EngineError> {
        let (view, card, message) = {
            let mut slot = self.inner.giveaway.lock().await;
            let g = slot.giveaway.as_mut().ok_or(NO_GIVEAWAY)?;
            if g.ended {
                return Err(EngineError::Conflict("This giveaway has ended.".into()));
            }
            match (redraw, g.winners.is_empty()) {
                (false, false) => {
                    return Err(EngineError::Conflict(
                        "A winner was already drawn. Use Redraw to pick another.".into(),
                    ));
                }
                (true, true) => return Err(EngineError::Conflict("Draw a winner first.".into())),
                _ => {}
            }
            if g.entrants.is_empty() {
                return Err(EngineError::Unavailable("Nobody has entered yet.".into()));
            }
            let Some(w) = g.draw(os_random)? else {
                return Err(EngineError::Unavailable("Everyone who entered has already won.".into()));
            };
            let message = g.winner_message(&w);
            slot.dirty = false;
            let g = slot.giveaway.as_ref().ok_or(NO_GIVEAWAY)?;
            (g.view(), g.card(), message)
        };
        self.broadcast_giveaway(Some((view.clone(), card)));
        self.announce(&message, "announce the winner");
        Ok(view)
    }

    /// Finishes it: the fees are spent, the winners stay on screen. 409 when already ended.
    pub async fn end_giveaway(&self) -> Result<GiveawayView, EngineError> {
        let (view, card, id) = {
            let mut slot = self.inner.giveaway.lock().await;
            let g = slot.giveaway.as_mut().ok_or(NO_GIVEAWAY)?;
            if g.ended {
                return Err(EngineError::Conflict("This giveaway has already ended.".into()));
            }
            g.ended = true;
            g.open = false;
            (g.view(), g.card(), g.id.clone())
        };
        self.db(move |s| repo::giveaways::settle(s, &id)).await?;
        self.broadcast_giveaway(Some((view.clone(), card)));
        Ok(view)
    }

    /// Cancels it and gives every entry fee back. Returns how many people were refunded.
    pub async fn cancel_giveaway(&self) -> Result<usize, EngineError> {
        let g = self.inner.giveaway.lock().await.giveaway.take().ok_or(NO_GIVEAWAY)?;
        let id = g.id.clone();
        let refunded = if g.ended { 0 } else { self.db(move |s| repo::giveaways::refund(s, Some(&id))).await? };
        self.broadcast_giveaway(None);
        Ok(refunded)
    }

    pub(crate) fn broadcast_giveaway(&self, v: Option<(GiveawayView, GiveawayCard)>) {
        let (view, card) = v.map_or((None, None), |(v, c)| (Some(v), Some(c)));
        self.send_live(LiveFrame::new("giveaway", &json!({ "giveaway": view })));
        self.send_overlay(OverlayFrame::new(
            Audience::Widget(WidgetKind::Giveaway),
            self.next_seq(),
            "state",
            &json!({ "giveaway": card }),
        ));
    }
}
