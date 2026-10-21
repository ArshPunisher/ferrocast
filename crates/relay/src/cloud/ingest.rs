//! Events into cloud mode (docs/CLOUD.md §1): the platform hooks (Twitch EventSub, Kick
//! webhooks, verified tips), a bounded queue, and the alert pipeline on an account's bus.
//!
//! The webhooks answer at once whatever happens here (Twitch and Kick never unsubscribe us):
//! an event is put on a queue of 4,096 and dropped and counted when it's full. One worker
//! takes them in order, so an account's alerts never overtake each other. Everything it does
//! is in memory (the rules, the alert design in use and the settings are read once per bus
//! and again after a change), so one worker keeps up with the free server's budget (§8).
//!
//! The pipeline is the engine's, with the same core functions: dedupe by the platform's id
//! (R-29), `alert::select` → `alert::render` → the alert design's `design::pick` /
//! `render_onto` → the account's `AlertQueue`, chat to the chat widgets, deletes and bans
//! taking lines off, labels from this stream's numbers.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use ferrocast_core::event::{Actor, Event, EventKind, Millis, Origin, PaymentStatus, Platform, TipProvider};
use ferrocast_core::money::Money;
use ferrocast_core::sample;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::bus::{Audience, Bus, Cached, platform_key};
use super::docs;
use crate::{AppState, now_ms};

/// Events waiting for the worker (docs/CLOUD.md §8).
pub const QUEUE: usize = 4_096;

#[derive(Debug)]
pub(crate) struct Job {
    account: String,
    event: Event,
}

fn sender(st: &AppState) -> &mpsc::Sender<Job> {
    st.cloud.ingest.get_or_init(|| {
        let (tx, rx) = mpsc::channel(QUEUE);
        tokio::spawn(worker(st.clone(), rx));
        tokio::spawn(reaper(st.clone()));
        tx
    })
}

/// Hands a normalised event to an account's bus (any producer: the platform hooks below,
/// test alerts, and cloud YouTube, Pro, built separately: docs/CLOUD.md §11). Never waits.
/// `false` when the queue was full and the event was dropped (counted).
pub(crate) fn publish(st: &AppState, account: &str, event: Event) -> bool {
    if sender(st).try_send(Job { account: account.to_string(), event }).is_err() {
        let n = st.cloud.buses.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_power_of_two() {
            tracing::warn!(dropped = n, "the cloud ingest queue is full: events are being dropped");
        }
        return false;
    }
    true
}

async fn worker(st: AppState, mut rx: mpsc::Receiver<Job>) {
    while let Some(job) = rx.recv().await {
        st.cloud.buses.events.fetch_add(1, Ordering::Relaxed);
        let bus = match st.cloud.buses.open(&job.account, st.cloud.cfg.max_active) {
            Ok(b) => b,
            Err(_) => {
                st.cloud.buses.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        let Some(cached) = cached(&st, &bus) else { continue };
        if let Some(ev) = process(&bus, &cached, job.event) {
            // Goals (saved), then polls and giveaways (chat), as the engine does.
            super::goals::on_event(&st, &bus, &ev);
            super::community::on_event(&st, &bus, &ev);
        }
    }
}

/// Once a minute: buses nobody used for ten minutes are dropped.
async fn reaper(st: AppState) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        let n = st.cloud.buses.reap(now_ms());
        if n > 0 {
            tracing::debug!(dropped = n, "idle cloud buses dropped");
        }
    }
}

/// The documents the pipeline reads, loaded once per bus. `None` (the event is dropped)
/// when the account can't take events now: cloud mode off, asleep, or the account disabled.
pub(crate) fn cached(st: &AppState, bus: &Bus) -> Option<Arc<Cached>> {
    if let Some(c) = bus.state().cache.clone() {
        return Some(c);
    }
    if !super::bot_allowed(&st.db, &bus.account) {
        return None;
    }
    let rules = docs::rules(st, &bus.account).ok()?;
    let settings = docs::settings(st, &bus.account).unwrap_or_default();
    let design = docs::active_alert_design(st, &bus.account);
    let c = Arc::new(Cached { rules, design, settings });
    bus.state().cache = Some(c.clone());
    Some(c)
}

/// One event through the pipeline; the event as it went out (`None`: dropped, a repeat, or
/// only a viewer count).
pub(crate) fn process(bus: &Bus, cached: &Cached, mut e: Event) -> Option<Arc<Event>> {
    let at = Millis(now_ms());
    e.id = docs::new_id();
    e.received_at = at;
    if e.occurred_at.0 == 0 {
        e.occurred_at = at;
    }
    // Never sent to overlays or the dashboard.
    e.raw = None;
    if e.origin != Origin::Test
        && let Some(src) = &e.source_id
        && !bus.state().first_time(&format!("{}:{src}", platform_key(e.platform)))
    {
        return None;
    }
    bus.touch();
    e.seq = bus.state().next_seq();
    match &e.kind {
        EventKind::ViewerCount { count } => {
            let viewers = {
                let mut st = bus.state();
                st.viewers.insert(platform_key(e.platform), *count);
                st.viewers.clone()
            };
            bus.send_overlay(Audience::All, "state", &json!({ "viewers": viewers }));
            return None;
        }
        EventKind::Connector(_) => return None,
        EventKind::StreamOnline { .. } => {
            let mut st = bus.state();
            let anyone_live = st.live.values().any(|l| *l);
            st.live.insert(platform_key(e.platform), true);
            if !anyone_live {
                st.session = super::bus::Session::default();
            }
        }
        EventKind::StreamOffline => {
            let mut st = bus.state();
            st.live.insert(platform_key(e.platform), false);
            st.viewers.remove(&platform_key(e.platform));
        }
        EventKind::MessageDeleted { message_id } => {
            let p = e.platform;
            bus.state().chat.retain(|c| !(c.platform == p && c.source_id.as_deref() == Some(message_id.as_str())));
            bus.send_overlay(
                Audience::Chat,
                "delete_message",
                &json!({ "platform": e.platform, "message_id": message_id }),
            );
        }
        EventKind::UserMessagesCleared { target } | EventKind::Ban { target, .. } => {
            if let Some(uid) = &target.id {
                let p = e.platform;
                bus.state().chat.retain(|c| {
                    !(c.platform == p && c.kind.actor().and_then(|a| a.id.as_deref()) == Some(uid.as_str()))
                });
            }
            bus.send_overlay(Audience::Chat, "delete_user", &json!({ "platform": e.platform, "user_id": target.id }));
        }
        _ => {}
    }

    if e.kind.is_alertable() || matches!(e.kind, EventKind::Tip { .. }) {
        support_event(bus, cached, &e);
    }

    let event = Arc::new(e);
    bus.send_dashboard("event", &*event);
    match event.kind {
        EventKind::ChatMessage { .. } => {
            // Chat is the bulk of the load: kept and sent only while something shows it (§8).
            let wanted = {
                let st = bus.state();
                st.chat_sockets > 0 || st.dashboards > 0
            };
            if wanted {
                bus.state().push_chat(event.clone());
                bus.send_overlay(Audience::Chat, "event", &*event);
            }
        }
        _ if event.kind.is_alertable() && event.origin == Origin::Live => {
            bus.state().push_event(event.clone());
            bus.send_overlay(Audience::Alerts, "event", &*event);
        }
        _ => {}
    }
    Some(event)
}

/// Labels, then the alert (when a rule covers it), as the engine does.
fn support_event(bus: &Bus, cached: &Cached, e: &Event) {
    if e.origin == Origin::Live {
        bus.state().session.record(e);
        bus.send(Audience::Labels, Arc::from(""));
    }
    if !e.may_alert() || (e.origin == Origin::Test && !cached.settings.test_alerts) {
        return;
    }
    let Some((rule, variation)) = ferrocast_core::alert::select(&cached.rules, e) else { return };
    let mut alert = ferrocast_core::alert::render(&docs::new_id(), rule, variation, e);
    // TTS in the cloud is the alert design's own switch (no local voices, no approval queue):
    // the overlay reads `tts_text` with the browser's voice.
    let mut speak = false;
    let test_design =
        e.source_id.as_deref().filter(|_| e.origin == Origin::Test).and_then(|k| bus.state().take_test_design(k));
    let design = test_design.as_ref().or(cached.design.as_ref());
    if let Some(d) = design
        && let Some((key, dv)) = ferrocast_core::design::pick(d, e)
    {
        ferrocast_core::design::render_onto(&mut alert, d, key, dv, e);
        speak = ferrocast_core::design::allows_tts(&dv.look, e);
    }
    alert.tts_text = if speak { tts_text(e) } else { None };
    let priority = if e.origin == Origin::Test { 0 } else { variation.priority };
    bus.enqueue(alert, priority);
}

/// What the browser reads: the message only (never numbers, links or names, R-48), cut short.
fn tts_text(e: &Event) -> Option<String> {
    let policy = ferrocast_core::tts::TtsPolicy {
        enabled: true,
        hold_for_approval: false,
        min_amounts: Vec::new(),
        min_platform_value: 0,
        ..Default::default()
    };
    match ferrocast_core::tts::decide(&policy, &ferrocast_core::filter::FilterConfig::default(), e) {
        ferrocast_core::tts::Decision::Speak { text } => Some(text),
        _ => None,
    }
}

// ── Test alerts ──────────────────────────────────────────────────────────────────────

/// A test alert through the cloud pipeline (`POST /c/api/v1/test`): the engine's samples,
/// an amount, and optionally the alert design to show it with (instead of the one in use).
/// `Err` is the message for a 422.
pub(crate) fn test_event(
    bus: &Bus,
    kind: &str,
    platform: Option<Platform>,
    amount: Option<f64>,
    design: Option<ferrocast_core::design::Design>,
) -> Result<Event, String> {
    let at = Millis(now_ms());
    let mut event = match platform {
        Some(p) => sample::sample(kind, p, at),
        None => sample::any_sample(kind, at),
    }
    .ok_or_else(|| format!("kind: there's no test for \"{kind}\" on that platform."))?;
    if let Some(amount) = amount {
        set_amount(&mut event, amount)?;
    }
    if let Some(d) = design {
        // The pipeline finds the design again by this one-use key.
        let key = format!("design-test-{}", docs::new_id());
        event.source_id = Some(key.clone());
        bus.state().put_test_design(&key, d);
    }
    Ok(event)
}

fn set_amount(e: &mut Event, amount: f64) -> Result<(), String> {
    if !amount.is_finite() || amount <= 0.0 || amount > 1_000_000_000.0 {
        return Err("amount: must be a number above 0 and up to 1,000,000,000.".into());
    }
    let whole = || u32::try_from(amount.round() as i64).unwrap_or(u32::MAX).max(1);
    match &mut e.kind {
        EventKind::Tip { money, .. } | EventKind::PaidMessage { money, .. } => {
            let digits = ferrocast_core::money::exponent(&money.currency);
            let minor = (amount * 10f64.powi(i32::try_from(digits).unwrap_or(2))).round() as i64;
            *money = Money::new(minor.max(1), &money.currency);
        }
        EventKind::PlatformGift { value, .. } => value.amount = u64::from(whole()),
        EventKind::GiftBatch { count, .. } => *count = whole(),
        EventKind::Raid { viewers, .. } => *viewers = whole(),
        EventKind::Subscription { months, .. } => *months = whole().min(1200),
        _ => return Err("amount: this kind of alert has no amount.".into()),
    }
    Ok(())
}

/// A sample for an overlay in test mode: an event and, when a rule covers it, its alert.
/// Nothing touches the real queue.
pub(crate) fn preview(cached: &Cached, n: u64, seq: u64) -> (Event, Option<ferrocast_core::alert::RenderedAlert>) {
    const KINDS: [&str; 6] = ["follow", "tip", "chat", "subscription", "raid", "platform_gift"];
    let at = Millis(now_ms());
    let kind = KINDS[(n as usize) % KINDS.len()];
    let chat = || {
        let mut actor = Actor::named(Platform::Twitch, "PreviewViewer");
        actor.id = Some("preview".into());
        Event::live(
            Platform::Twitch,
            None,
            at,
            EventKind::ChatMessage {
                actor,
                content: ferrocast_core::event::Content::plain("This is how chat looks on stream."),
                reply_to: None,
                is_action: false,
            },
        )
    };
    let mut e = if kind == "chat" { chat() } else { sample::any_sample(kind, at).unwrap_or_else(chat) };
    e.id = format!("preview-{n}");
    e.origin = Origin::Test;
    e.seq = seq;
    let alert = e.may_alert().then(|| ferrocast_core::alert::select(&cached.rules, &e)).flatten().map(|(rule, v)| {
        let mut a = ferrocast_core::alert::render(&format!("preview-{n}"), rule, v, &e);
        if let Some(d) = &cached.design
            && let Some((key, dv)) = ferrocast_core::design::pick(d, &e)
        {
            ferrocast_core::design::render_onto(&mut a, d, key, dv, &e);
        }
        a
    });
    (e, alert)
}

// ── The platform hooks ───────────────────────────────────────────────────────────────

/// A verified Twitch EventSub notification (`bot::twitch::eventsub`): for a channel bound
/// to a cloud account, mapped with the desktop's own mapper.
pub(crate) fn twitch(st: &AppState, message_id: &str, v: &Value) {
    let kind = v.pointer("/subscription/type").and_then(Value::as_str).unwrap_or("");
    if !super::connect::TWITCH_KINDS.contains(&kind) {
        return;
    }
    let cond = v.pointer("/subscription/condition");
    let Some(b) = cond
        .and_then(|c| c.get("broadcaster_user_id").or_else(|| c.get("to_broadcaster_user_id")))
        .and_then(Value::as_str)
    else {
        return;
    };
    let Ok(Some(ch)) = st.db.cloud_channel("twitch", b) else { return };
    let event = v.get("event").cloned().unwrap_or(Value::Null);
    let e = ferrocast_twitch::map::map(kind, message_id, &event, Millis(now_ms()));
    publish(st, &ch.account_id, e);
}

/// A verified Kick webhook (`crate::kick_webhook`) for a channel bound to a cloud account;
/// the account's cloud bot reads its chat, as for a Pro PC's channel.
pub(crate) fn kick(st: &AppState, broadcaster: &str, ty: &str, message_id: &str, body: &Value) {
    if !super::connect::KICK_EVENTS.contains(&ty) {
        return;
    }
    let Ok(Some(ch)) = st.db.cloud_channel("kick", broadcaster) else { return };
    let e = ferrocast_kick::map::map(ty, message_id, body, Millis(now_ms()));
    if matches!(ty, "chat.message.sent" | "livestream.status.updated")
        && let Ok(Some(bot)) = st.db.bot_channel(&ch.account_id, "kick")
    {
        let (st2, e2, ty2, body2) = (st.clone(), e.clone(), ty.to_string(), body.clone());
        st.bot.work_kick(Box::pin(async move {
            if ty2 == "livestream.status.updated" {
                let live = body2.get("is_live").and_then(Value::as_bool).unwrap_or(false);
                let at = body2.get("started_at").and_then(Value::as_str).and_then(crate::bot::twitch::parse_time);
                crate::bot::set_live(&st2, &bot.sub_id, Platform::Kick, live.then(|| at.unwrap_or_else(now_ms)));
            } else {
                crate::bot::on_chat(&st2, &bot, Platform::Kick, e2).await;
            }
        }));
    }
    publish(st, &ch.account_id, e);
}

/// A YouTube event the cloud bot's reader got for a subscription attached to `account`
/// (Pro, §11): published while the account has cloud mode on and awake. Reading it cost the
/// subscription's YouTube units already; publishing costs none.
pub(crate) fn youtube(st: &AppState, account: &str, e: &Event) {
    if e.platform != Platform::Youtube || !super::bot_allowed(&st.db, account) {
        return;
    }
    publish(st, account, e.clone());
}

/// A verified tip for an install (`tips::queue_tip`): published on the account's bus when
/// the install is the account's cloud install or a PC signed in to an account with cloud
/// mode on (§7). Only captured payments alert (R-49).
/// `false` only when a tip for a cloud install couldn't be taken now (the ingest queue was
/// full): the caller lets the provider retry, since no PC queue holds it either.
pub(crate) fn tip(st: &AppState, install: &str, p: &Value) -> bool {
    let cloud_install = st.db.cloud_account_by_install(install).ok().flatten();
    let account = match &cloud_install {
        Some(a) => Some(a.clone()),
        None => st.db.account_for_install(install, now_ms()).ok().flatten(),
    };
    let Some(account) = account else { return true };
    let Ok(Some(c)) = st.db.cloud_account(&account) else { return true };
    if c.sleeping_at.is_some() {
        if cloud_install.is_none() {
            return true;
        }
        // Money arriving on the account's own tip page is use: it wakes (§5).
        super::touch(st, &c);
    }
    match tip_event(p) {
        Some(e) => publish(st, &account, e) || cloud_install.is_none(),
        None => true,
    }
}

pub(crate) fn tip_event(p: &Value) -> Option<Event> {
    let provider_key = p["provider"].as_str()?;
    let provider = match provider_key {
        "razorpay" => TipProvider::Razorpay,
        "stripe" => TipProvider::Stripe,
        "ko_fi" => TipProvider::KoFi,
        "paypal" => TipProvider::Paypal,
        _ => return None,
    };
    let payment = p["payment_id"].as_str().filter(|s| !s.is_empty())?;
    if p["status"].as_str()? != "captured" {
        return None;
    }
    let currency = p["currency"].as_str()?;
    let money = match (p["amount_minor"].as_i64(), p["digits"].as_u64()) {
        (Some(minor), Some(digits)) => Money::from_scaled(minor, u32::try_from(digits).ok()?, currency).ok()?,
        _ => Money::parse_decimal_lenient(p["amount"].as_str()?, currency).ok()?,
    };
    if money.minor <= 0 {
        return None;
    }
    let name = p["name"].as_str().filter(|n| !n.trim().is_empty()).unwrap_or("Someone");
    let mut actor = Actor::named(Platform::Tips, name);
    actor.login = None;
    Some(Event::live(
        Platform::Tips,
        Some(format!("{provider_key}:{payment}")),
        Millis(now_ms()),
        EventKind::Tip {
            actor,
            money,
            message: p["message"].as_str().filter(|m| !m.trim().is_empty()).map(String::from),
            provider,
            payment_ref: payment.to_string(),
            status: PaymentStatus::Captured,
        },
    ))
}
