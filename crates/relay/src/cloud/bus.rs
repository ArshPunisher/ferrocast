//! The per-account event bus (docs/CLOUD.md §1): in memory only, made when an account
//! becomes active (an overlay or dashboard socket opens, or a live event arrives) and dropped
//! ten minutes after its last socket closed and no event came.
//!
//! A bus holds a `broadcast` channel of frames (64, each serialised once and shared as an
//! `Arc<str>`), the last 25 alertable events, the last 50 chat lines, viewer counts, the
//! session labels and the alert queue (`core::queue::AlertQueue`, driven by the overlays'
//! acks as on the PC). Nothing here is written to disk (R-51): a relay restart starts empty.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use ferrocast_core::alert::{AlertRule, RenderedAlert};
use ferrocast_core::design::Design;
use ferrocast_core::event::{Event, EventKind, Millis, Platform};
use ferrocast_core::money::Money;
use ferrocast_core::overlay::{Overlay, WidgetKind};
use ferrocast_core::queue::{Action, AlertQueue, QueueTuning};
use ferrocast_core::template::{Template, Vars};
use serde_json::{Value, json};
use tokio::sync::{Notify, broadcast};

use super::docs::Settings;
use crate::now_ms;

/// Frames a socket may fall behind by before it gets a fresh snapshot.
pub const FRAMES: usize = 64;
pub const EVENTS_KEPT: usize = 25;
pub const CHAT_KEPT: usize = 50;
/// Platform event ids remembered for the dedupe (R-29).
const SEEN_KEPT: usize = 512;
/// A bus with no socket and no event for this long is dropped.
pub const IDLE_DROP_MS: i64 = 10 * 60_000;
/// Overlay sockets per account, and per overlay (docs/CLOUD.md §5).
pub const SOCKETS_PER_ACCOUNT: usize = 20;
pub const SOCKETS_PER_OVERLAY: usize = 4;
/// The longest the queue's timer sleeps without looking.
const MAX_IDLE: Duration = Duration::from_secs(30);

/// Which sockets need a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Audience {
    /// Every overlay.
    All,
    /// Overlays with a chat widget.
    Chat,
    /// Overlays with an alerts or events widget.
    Alerts,
    /// One overlay (its layout changed, or its URL was revoked).
    Overlay(String),
    /// The session labels changed: each overlay renders its own label widgets.
    Labels,
    /// The web dashboard's live socket.
    Dashboard,
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub audience: Audience,
    pub text: Arc<str>,
}

/// `{ v: 1, seq?, type, data }` (API.md §5, §6).
pub fn envelope(seq: Option<u64>, kind: &str, data: &impl serde::Serialize) -> Arc<str> {
    let data = serde_json::to_value(data).unwrap_or(Value::Null);
    let v = match seq {
        Some(seq) => json!({ "v": 1, "seq": seq, "type": kind, "data": data }),
        None => json!({ "v": 1, "type": kind, "data": data }),
    };
    Arc::from(v.to_string())
}

pub fn platform_key(p: Platform) -> String {
    serde_json::to_value(p).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
}

/// This stream's numbers for the label widgets (the engine's `Session`, in memory).
#[derive(Debug, Clone, Default)]
pub struct Session {
    pub follows: u32,
    pub subs: u32,
    pub latest_follower: Option<String>,
    pub latest_sub: Option<String>,
    pub latest_tip: Option<(String, Money)>,
    pub top_tip: Option<(String, Money)>,
}

impl Session {
    pub fn record(&mut self, event: &Event) {
        match &event.kind {
            EventKind::Follow { actor } => {
                self.follows += 1;
                self.latest_follower = Some(actor.display_name.clone());
            }
            EventKind::Subscription { actor, gifted_by: None, .. } => {
                self.subs += 1;
                self.latest_sub = Some(actor.display_name.clone());
            }
            EventKind::GiftBatch { gifter, count, .. } => {
                self.subs = self.subs.saturating_add(*count);
                self.latest_sub = Some(gifter.display_name.clone());
            }
            EventKind::Tip { actor, money, .. } | EventKind::PaidMessage { actor, money, .. }
                if event.kind.is_alertable() =>
            {
                self.latest_tip = Some((actor.display_name.clone(), money.clone()));
                let beats = match &self.top_tip {
                    Some((_, top)) => top.currency == money.currency && money.minor > top.minor,
                    None => true,
                };
                if beats {
                    self.top_tip = Some((actor.display_name.clone(), money.clone()));
                }
            }
            _ => {}
        }
    }

    pub fn label(&self, source: &str) -> String {
        match source {
            "latest_follower" => self.latest_follower.clone().unwrap_or_default(),
            "latest_sub" => self.latest_sub.clone().unwrap_or_default(),
            "latest_tip" => self.latest_tip.as_ref().map(|(n, m)| format!("{n} {}", m.display())).unwrap_or_default(),
            "top_tip_session" => self.top_tip.as_ref().map(|(n, m)| format!("{n} {}", m.display())).unwrap_or_default(),
            "session_follows" => self.follows.to_string(),
            "session_subs" => self.subs.to_string(),
            _ => String::new(),
        }
    }

    /// The label widgets of an overlay, rendered.
    pub fn labels(&self, o: &Overlay) -> serde_json::Map<String, Value> {
        o.widgets
            .iter()
            .filter(|w| w.kind == WidgetKind::Label)
            .map(|w| {
                let source = w.config.get("source").and_then(Value::as_str).unwrap_or("");
                let tpl = w.config.get("template").and_then(Value::as_str).unwrap_or("{value}");
                let mut vars = Vars::new();
                vars.insert("value".into(), self.label(source));
                let text = Template::parse(tpl).map(|t| t.render(&vars)).unwrap_or_default();
                (w.id.clone(), json!(text))
            })
            .collect()
    }
}

/// What the pipeline reads for every event: the rules, the alert design in use and the
/// settings. Loaded once and dropped whenever a document changes.
#[derive(Debug, Default)]
pub struct Cached {
    pub rules: Vec<AlertRule>,
    pub design: Option<Design>,
    pub settings: Settings,
}

#[derive(Debug)]
pub struct BusState {
    pub seq: u64,
    pub events: VecDeque<Arc<Event>>,
    pub chat: VecDeque<Arc<Event>>,
    pub viewers: BTreeMap<String, u64>,
    pub session: Session,
    pub queue: AlertQueue,
    seen: VecDeque<String>,
    seen_set: HashSet<String>,
    /// Open overlay sockets per overlay.
    pub sockets: HashMap<String, usize>,
    /// Open overlay sockets that show chat, and dashboard sockets.
    pub chat_sockets: usize,
    pub dashboards: usize,
    pub cache: Option<Arc<Cached>>,
    pub live: BTreeMap<String, bool>,
    /// Designs the designer's "Send test alert" asked for, by the test event's key.
    test_designs: VecDeque<(String, Design)>,
}

impl BusState {
    pub fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    pub fn socket_count(&self) -> usize {
        self.sockets.values().sum()
    }

    /// `false` when this platform id was seen already (a retry or a replay).
    pub fn first_time(&mut self, key: &str) -> bool {
        if self.seen_set.contains(key) {
            return false;
        }
        if self.seen.len() >= SEEN_KEPT
            && let Some(old) = self.seen.pop_front()
        {
            self.seen_set.remove(&old);
        }
        self.seen.push_back(key.to_string());
        self.seen_set.insert(key.to_string());
        true
    }

    pub fn put_test_design(&mut self, key: &str, d: Design) {
        if self.test_designs.len() >= 8 {
            self.test_designs.pop_front();
        }
        self.test_designs.push_back((key.to_string(), d));
    }

    pub fn take_test_design(&mut self, key: &str) -> Option<Design> {
        let i = self.test_designs.iter().position(|(k, _)| k == key)?;
        self.test_designs.remove(i).map(|(_, d)| d)
    }

    pub fn push_event(&mut self, e: Arc<Event>) {
        if self.events.len() >= EVENTS_KEPT {
            self.events.pop_front();
        }
        self.events.push_back(e);
    }

    pub fn push_chat(&mut self, e: Arc<Event>) {
        if self.chat.len() >= CHAT_KEPT {
            self.chat.pop_front();
        }
        self.chat.push_back(e);
    }
}

/// One account's bus.
#[derive(Debug)]
pub struct Bus {
    pub account: String,
    tx: broadcast::Sender<Frame>,
    state: Mutex<BusState>,
    wake: Notify,
    last_used: AtomicI64,
}

/// The queue as the dashboard shows it (API.md `GET /api/v1/queue`).
pub fn queue_view(q: &AlertQueue) -> Value {
    json!({
        "paused": q.is_paused(),
        "playing": q.playing().map(|p| json!({ "alert": p.alert, "started_at": p.started_at.0 })),
        "waiting": q.waiting().map(|w| json!({ "alert": w.alert, "priority": w.priority, "enqueued_at": w.enqueued_at.0 })).collect::<Vec<_>>(),
    })
}

impl Bus {
    fn new(account: &str) -> Bus {
        let (tx, _) = broadcast::channel(FRAMES);
        Bus {
            account: account.to_string(),
            tx,
            state: Mutex::new(BusState {
                seq: 0,
                events: VecDeque::with_capacity(EVENTS_KEPT),
                chat: VecDeque::with_capacity(CHAT_KEPT),
                viewers: BTreeMap::new(),
                session: Session::default(),
                queue: AlertQueue::new(QueueTuning::default()),
                seen: VecDeque::new(),
                seen_set: HashSet::new(),
                sockets: HashMap::new(),
                chat_sockets: 0,
                dashboards: 0,
                cache: None,
                live: BTreeMap::new(),
                test_designs: VecDeque::new(),
            }),
            wake: Notify::new(),
            last_used: AtomicI64::new(now_ms()),
        }
    }

    pub fn state(&self) -> MutexGuard<'_, BusState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Frame> {
        self.tx.subscribe()
    }

    pub fn send(&self, audience: Audience, text: Arc<str>) {
        let _ = self.tx.send(Frame { audience, text });
    }

    /// An overlay frame with the next seq.
    pub fn send_overlay(&self, audience: Audience, kind: &str, data: &impl serde::Serialize) {
        let seq = self.state().next_seq();
        self.send(audience, envelope(Some(seq), kind, data));
    }

    pub fn send_dashboard(&self, kind: &str, data: &impl serde::Serialize) {
        if self.tx.receiver_count() > 0 {
            self.send(Audience::Dashboard, envelope(None, kind, data));
        }
    }

    pub fn touch(&self) {
        self.last_used.store(now_ms(), Ordering::Relaxed);
    }

    pub fn idle_since(&self) -> i64 {
        self.last_used.load(Ordering::Relaxed)
    }

    /// Runs a queue operation and carries out what it says: alerts to the overlays, hides,
    /// a notice for folded ones, and the queue to the dashboard when it changed.
    pub fn queue_op(&self, op: impl FnOnce(&mut AlertQueue, Millis) -> Vec<Action>) -> Value {
        let (actions, changed, view) = {
            let mut st = self.state();
            let before = st.queue.clone();
            let actions = op(&mut st.queue, Millis(now_ms()));
            let changed = st.queue != before;
            (actions, changed, queue_view(&st.queue))
        };
        let any = !actions.is_empty();
        for a in actions {
            match a {
                Action::Show { alert } => self.send_overlay(Audience::Alerts, "alert", &alert),
                Action::Hide { alert_id } => {
                    self.send_overlay(Audience::Alerts, "alert_hide", &json!({ "alert_id": alert_id }));
                }
                Action::Folded { alert_ids } => self.send_dashboard(
                    "notice",
                    &json!({ "key": "alerts-folded", "level": "info", "message": format!(
                        "{} alert{} waited too long and went to the event list instead.",
                        alert_ids.len(),
                        if alert_ids.len() == 1 { "" } else { "s" }
                    ) }),
                ),
            }
        }
        if changed {
            self.send_dashboard("queue", &view);
        }
        if changed || any {
            self.wake.notify_one();
        }
        view
    }

    pub fn enqueue(&self, alert: RenderedAlert, priority: i32) {
        self.queue_op(move |q, t| q.enqueue(alert, priority, t));
    }
}

/// Moves the queue on: alerts that never finished time out, the next one starts after the
/// gap. One task per bus; it ends soon after the bus is dropped.
async fn timer(w: Weak<Bus>) {
    loop {
        let Some(bus) = w.upgrade() else { return };
        let wake = bus.state().queue.next_wake();
        let sleep = match wake {
            Some(at) => Duration::from_millis(u64::try_from((at.0 - now_ms()).max(0)).unwrap_or(0)).min(MAX_IDLE),
            None => MAX_IDLE,
        };
        tokio::select! {
            _ = bus.wake.notified() => {}
            _ = tokio::time::sleep(sleep) => {}
        }
        bus.queue_op(|q, t| q.tick(t));
    }
}

/// Why a bus couldn't be had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    /// `CLOUD_MAX_ACTIVE` buses are in memory and none of them is idle.
    Full,
}

/// Every bus in memory, and the relay-wide socket count.
#[derive(Debug, Default)]
pub struct Buses {
    map: Mutex<HashMap<String, Arc<Bus>>>,
    pub sockets: AtomicUsize,
    /// Events dropped (the ingest queue was full, or no bus could be had).
    pub dropped: AtomicU64,
    /// Events taken in since the relay started, for the admin numbers.
    pub events: AtomicU64,
    /// `events` at the last hourly sweep, and how many came in the hour before it.
    pub events_mark: AtomicU64,
    pub events_last_hour: AtomicU64,
}

impl Buses {
    fn map(&self) -> MutexGuard<'_, HashMap<String, Arc<Bus>>> {
        self.map.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The bus of an account, if it's in memory.
    pub fn get(&self, account: &str) -> Option<Arc<Bus>> {
        self.map().get(account).cloned()
    }

    /// The bus of an account, made if needed. With `max` buses in memory the least recently
    /// used one without a socket is dropped; if every one has a socket, `Busy::Full`.
    pub fn open(&self, account: &str, max: usize) -> Result<Arc<Bus>, Busy> {
        let mut map = self.map();
        if let Some(b) = map.get(account) {
            b.touch();
            return Ok(b.clone());
        }
        if map.len() >= max.max(1) {
            let idle = map
                .iter()
                .filter(|(_, b)| b.state().socket_count() == 0 && b.state().dashboards == 0)
                .min_by_key(|(_, b)| b.idle_since())
                .map(|(k, _)| k.clone());
            match idle {
                Some(k) => {
                    map.remove(&k);
                }
                None => return Err(Busy::Full),
            }
        }
        let bus = Arc::new(Bus::new(account));
        map.insert(account.to_string(), bus.clone());
        drop(map);
        tokio::spawn(timer(Arc::downgrade(&bus)));
        Ok(bus)
    }

    /// Drops buses with no socket that saw nothing for [`IDLE_DROP_MS`].
    pub fn reap(&self, now: i64) -> usize {
        let mut map = self.map();
        let before = map.len();
        map.retain(|_, b| {
            let st = b.state();
            st.socket_count() > 0 || st.dashboards > 0 || now - b.idle_since() < IDLE_DROP_MS
        });
        before - map.len()
    }

    /// Drops an account's bus at once (it slept, was disabled, or turned cloud mode off).
    pub fn drop_bus(&self, account: &str) -> Option<Arc<Bus>> {
        self.map().remove(account)
    }

    pub fn active(&self) -> usize {
        self.map().len()
    }

    /// The documents an account's pipeline reads changed: read them again next time.
    pub fn forget_cache(&self, account: &str) {
        if let Some(b) = self.get(account) {
            b.state().cache = None;
        }
    }
}
