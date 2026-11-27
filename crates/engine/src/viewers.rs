//! Live viewer-count polling for Twitch, YouTube and Kick while on air.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ferrocast_connector::{Connector, ConnectorError};
use ferrocast_core::event::{Event, EventKind, Platform};
use tokio::sync::Notify;

use crate::{Engine, now};

const PLATFORMS: [Platform; 3] = [Platform::Twitch, Platform::Youtube, Platform::Kick];
const INITIAL_DELAY: Duration = Duration::from_secs(10);
const POLL_TWITCH: Duration = Duration::from_secs(30);
const POLL_YOUTUBE: Duration = Duration::from_secs(60);
const POLL_KICK: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(300);
const AUTH_BACKOFF: Duration = Duration::from_secs(300);

fn platform_index(p: Platform) -> Option<usize> {
    match p {
        Platform::Twitch => Some(0),
        Platform::Youtube => Some(1),
        Platform::Kick => Some(2),
        _ => None,
    }
}

fn poll_interval(platform: Platform) -> Duration {
    match platform {
        Platform::Youtube => POLL_YOUTUBE,
        Platform::Kick => POLL_KICK,
        _ => POLL_TWITCH,
    }
}

fn backoff_delay(platform: Platform, failures: u32) -> Duration {
    let shift = failures.min(4);
    poll_interval(platform).saturating_mul(1u32 << shift).min(MAX_BACKOFF)
}

pub(crate) trait Clock: Send + Sync {
    fn now(&self) -> Instant;
    fn wait_until(&self, deadline: Option<Instant>) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

struct TokioClock;

impl Clock for TokioClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn wait_until(&self, deadline: Option<Instant>) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            match deadline {
                Some(dl) => tokio::time::sleep_until(tokio::time::Instant::from_std(dl)).await,
                None => std::future::pending::<()>().await,
            }
        })
    }
}

#[derive(Debug, Default, Clone)]
struct PlatformSlot {
    next_poll: Option<Instant>,
    failures: u32,
    warned: bool,
    rate_limited_until: Option<Instant>,
}

#[derive(Debug, Default)]
pub(crate) struct PollSchedule {
    slots: [PlatformSlot; 3],
}

impl PollSchedule {
    pub(crate) fn sync_liveness(&mut self, active: [bool; 3], has_connector: [bool; 3], now: Instant) {
        for i in 0..3 {
            if let Some(until) = self.slots[i].rate_limited_until
                && now >= until
            {
                self.slots[i].rate_limited_until = None;
            }
            if !has_connector[i] {
                self.slots[i] = PlatformSlot::default();
                continue;
            }
            if active[i] {
                if self.slots[i].next_poll.is_none() {
                    let default_due = now + INITIAL_DELAY;
                    self.slots[i].next_poll =
                        Some(self.slots[i].rate_limited_until.map_or(default_due, |rl| rl.max(default_due)));
                }
            } else {
                let rate_limited_until = self.slots[i].rate_limited_until;
                self.slots[i] = PlatformSlot { rate_limited_until, ..PlatformSlot::default() };
            }
        }
    }

    pub(crate) fn note_rate_limit(&mut self, idx: usize, retry_after: Duration, now: Instant) {
        let wait = retry_after.max(backoff_delay(PLATFORMS[idx], self.slots[idx].failures.max(1)));
        let until = now + wait;
        self.slots[idx].rate_limited_until =
            Some(self.slots[idx].rate_limited_until.map_or(until, |prev| prev.max(until)));
    }

    pub(crate) fn reset_after_stale_poll(&mut self, idx: usize, now: Instant) {
        let rate_limited_until = self.slots[idx].rate_limited_until.filter(|rl| *rl > now);
        let default_due = now + INITIAL_DELAY;
        self.slots[idx] = PlatformSlot {
            next_poll: Some(rate_limited_until.map_or(default_due, |rl| rl.max(default_due))),
            failures: 0,
            warned: false,
            rate_limited_until,
        };
    }

    pub(crate) fn due_mask(&self, now: Instant) -> [bool; 3] {
        [
            self.slots[0].next_poll.is_some_and(|t| t <= now),
            self.slots[1].next_poll.is_some_and(|t| t <= now),
            self.slots[2].next_poll.is_some_and(|t| t <= now),
        ]
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.slots.iter().filter_map(|s| s.next_poll).min()
    }

    pub(crate) fn record_result(
        &mut self,
        idx: usize,
        res: &Result<Option<u64>, ConnectorError>,
        now: Instant,
    ) -> Option<u64> {
        let platform = PLATFORMS[idx];
        let slot = &mut self.slots[idx];
        match res {
            Ok(Some(count)) => {
                slot.failures = 0;
                slot.warned = false;
                slot.rate_limited_until = None;
                slot.next_poll = Some(now + poll_interval(platform));
                tracing::debug!(platform = %platform, count = *count, "viewer count");
                Some(*count)
            }
            Ok(None) => {
                slot.failures = 0;
                slot.warned = false;
                slot.rate_limited_until = None;
                slot.next_poll = Some(now + poll_interval(platform));
                tracing::debug!(platform = %platform, "viewer count unavailable while live");
                None
            }
            Err(err) => {
                slot.failures = slot.failures.saturating_add(1);
                let delay = match err {
                    ConnectorError::RateLimited { retry_after, .. } => {
                        let wait = (*retry_after).max(backoff_delay(platform, slot.failures));
                        slot.rate_limited_until = Some(now + wait);
                        wait
                    }
                    ConnectorError::SignInNeeded(_) | ConnectorError::MissingScope { .. } => {
                        slot.rate_limited_until = None;
                        AUTH_BACKOFF
                    }
                    _ => {
                        slot.rate_limited_until = None;
                        backoff_delay(platform, slot.failures)
                    }
                };
                if !slot.warned {
                    slot.warned = true;
                    tracing::warn!(
                        platform = %platform,
                        error = %err,
                        delay_s = delay.as_secs(),
                        "viewer count poll failed"
                    );
                } else {
                    tracing::debug!(
                        platform = %platform,
                        error = %err,
                        delay_s = delay.as_secs(),
                        "viewer count poll still failing"
                    );
                }
                slot.next_poll = Some(now + delay);
                None
            }
        }
    }
}

const GOLIVE_STARTUP_RECHECK: Duration = Duration::from_secs(2);
const GOLIVE_STARTUP_WINDOW: Duration = Duration::from_secs(60);
const GOLIVE_RETRY_RECHECK: Duration = Duration::from_secs(30);

fn service_platform(kind: ferrocast_golive::Service) -> Option<Platform> {
    match kind {
        ferrocast_golive::Service::Twitch => Some(Platform::Twitch),
        ferrocast_golive::Service::Youtube => Some(Platform::Youtube),
        ferrocast_golive::Service::Kick => Some(Platform::Kick),
        ferrocast_golive::Service::Custom => None,
    }
}

pub(crate) fn golive_active_platforms_from(
    running: &std::collections::BTreeSet<String>,
    destinations: &[crate::golive::DestinationConfig],
) -> [bool; 3] {
    let mut out = [false; 3];
    for id in running {
        let platform = destinations.iter().find(|d| d.id == *id).and_then(|d| service_platform(d.kind)).or_else(|| {
            if id == "twitch" {
                Some(Platform::Twitch)
            } else if id == "youtube" || id.starts_with("youtube-") || id.starts_with("youtube_") {
                Some(Platform::Youtube)
            } else if id == "kick" {
                Some(Platform::Kick)
            } else {
                None
            }
        });
        if let Some(idx) = platform.and_then(platform_index) {
            out[idx] = true;
        }
    }
    out
}

fn has_pending_golive_destination(
    has_conn: [bool; 3],
    active: [bool; 3],
    destinations: &[crate::golive::DestinationConfig],
) -> bool {
    destinations
        .iter()
        .filter(|d| d.enabled)
        .filter_map(|d| service_platform(d.kind))
        .any(|p| platform_index(p).is_some_and(|i| has_conn[i] && !active[i]))
}

#[cfg(test)]
pub(crate) fn effective_next_deadline(
    scheduled: Option<Instant>,
    golive_running: bool,
    has_conn: [bool; 3],
    active: [bool; 3],
    destinations: &[crate::golive::DestinationConfig],
    now: Instant,
) -> Option<Instant> {
    effective_next_deadline_since(scheduled, golive_running, has_conn, active, destinations, None, now)
}

pub(crate) fn effective_next_deadline_since(
    scheduled: Option<Instant>,
    golive_running: bool,
    has_conn: [bool; 3],
    active: [bool; 3],
    destinations: &[crate::golive::DestinationConfig],
    pending_since: Option<Instant>,
    now: Instant,
) -> Option<Instant> {
    if golive_running && (scheduled.is_none() || has_pending_golive_destination(has_conn, active, destinations)) {
        let interval = match pending_since {
            Some(since) if now.saturating_duration_since(since) >= GOLIVE_STARTUP_WINDOW => GOLIVE_RETRY_RECHECK,
            _ => GOLIVE_STARTUP_RECHECK,
        };
        let recheck = now + interval;
        Some(scheduled.map_or(recheck, |dl| dl.min(recheck)))
    } else {
        scheduled
    }
}

fn live_snapshot(engine: &Engine) -> ([bool; 3], [bool; 3]) {
    let st = engine.state();
    let golive_active = golive_active_platforms_from(&engine.golive_running(), &engine.golive_config().destinations);
    let mut active = [false; 3];
    let mut has_conn = [false; 3];
    for (i, p) in PLATFORMS.iter().enumerate() {
        let stream_live = st.live.get(p).copied().unwrap_or(false);
        active[i] = stream_live || golive_active[i];
        has_conn[i] = engine.connector(*p).is_some();
    }
    (active, has_conn)
}

pub(crate) fn clear_stopped_golive_viewers(engine: &Engine, prev_active: [bool; 3], now_active: [bool; 3]) {
    for (idx, &p) in PLATFORMS.iter().enumerate() {
        if !(prev_active[idx] && !now_active[idx]) {
            continue;
        }
        let cleared = {
            let mut st = engine.state_mut();
            if st.live.get(&p).copied().unwrap_or(false) {
                None
            } else {
                st.viewers.remove(&p).map(|_| overlay_clear_payload(&st.viewers, p))
            }
        };
        if let Some(viewers) = cleared {
            engine.send_overlay(crate::frames::OverlayFrame::new(
                crate::frames::Audience::All,
                engine.next_seq(),
                "state",
                &serde_json::json!({ "viewers": viewers }),
            ));
            let connectors = engine.state().connectors.clone();
            engine.send_live(crate::frames::LiveFrame::new(
                "status",
                &serde_json::json!({ "connectors": connectors, "viewers": viewers }),
            ));
        }
    }
}

async fn poll_one(c: Option<Arc<dyn Connector>>) -> Option<Result<Option<u64>, ConnectorError>> {
    Some(c?.viewer_count().await)
}

pub(crate) async fn run(engine: Engine) {
    let stop = Arc::new(Notify::new());
    run_with_clock(engine, Arc::new(TokioClock), stop).await;
}

pub(crate) async fn run_with_clock<C: Clock>(engine: Engine, clock: Arc<C>, stop: Arc<Notify>) {
    let mut schedule = PollSchedule::default();
    let mut prev_active = [false; 3];
    let mut golive_pending_since: Option<Instant> = None;
    let mut wake_rx = engine.inner.viewer_wake.subscribe();
    let _ = wake_rx.borrow_and_update();
    loop {
        let (active, has_conn) = live_snapshot(&engine);
        clear_stopped_golive_viewers(&engine, prev_active, active);
        prev_active = active;
        schedule.sync_liveness(active, has_conn, clock.now());

        let due = schedule.due_mask(clock.now());
        if due.iter().any(|d| *d) {
            let pre_epochs = engine.live_epochs();
            let targets: [Option<Arc<dyn Connector>>; 3] = [
                if due[0] { engine.connector(PLATFORMS[0]) } else { None },
                if due[1] { engine.connector(PLATFORMS[1]) } else { None },
                if due[2] { engine.connector(PLATFORMS[2]) } else { None },
            ];
            let (r0, r1, r2) =
                tokio::join!(poll_one(targets[0].clone()), poll_one(targets[1].clone()), poll_one(targets[2].clone()));
            let poll_end = clock.now();
            let post_epochs = engine.live_epochs();
            let cur_conns: [Option<Arc<dyn Connector>>; 3] =
                [engine.connector(PLATFORMS[0]), engine.connector(PLATFORMS[1]), engine.connector(PLATFORMS[2])];
            let (still_active, still_conn) = live_snapshot(&engine);
            clear_stopped_golive_viewers(&engine, prev_active, still_active);
            prev_active = still_active;
            schedule.sync_liveness(still_active, still_conn, poll_end);
            for (idx, maybe_res) in [r0, r1, r2].into_iter().enumerate() {
                let Some(res) = maybe_res else { continue };
                if !still_active[idx] || !still_conn[idx] {
                    continue;
                }
                let same_conn = match (&targets[idx], &cur_conns[idx]) {
                    (Some(sent), Some(cur)) => Arc::ptr_eq(sent, cur),
                    _ => false,
                };
                if post_epochs[idx] != pre_epochs[idx] || !same_conn {
                    if same_conn && let Err(ConnectorError::RateLimited { retry_after, .. }) = &res {
                        schedule.note_rate_limit(idx, *retry_after, poll_end);
                    }
                    schedule.reset_after_stale_poll(idx, poll_end);
                    continue;
                }
                if let Some(count) = schedule.record_result(idx, &res, poll_end) {
                    let ev = Event::live(PLATFORMS[idx], None, now(), EventKind::ViewerCount { count });
                    if engine.submit(ev).await.is_err() {
                        return;
                    }
                }
            }
        }

        let now_t = clock.now();
        let destinations = &engine.golive_config().destinations;
        let golive_running = engine.golive_stream_running();
        if golive_running && has_pending_golive_destination(has_conn, active, destinations) {
            golive_pending_since.get_or_insert(now_t);
        } else {
            golive_pending_since = None;
        }
        let next = effective_next_deadline_since(
            schedule.next_deadline(),
            golive_running,
            has_conn,
            active,
            destinations,
            golive_pending_since,
            now_t,
        );
        tokio::select! {
            _ = stop.notified() => return,
            changed = wake_rx.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            _ = clock.wait_until(next) => {},
        }
    }
}

pub(crate) fn overlay_clear_payload(
    remaining: &std::collections::HashMap<Platform, u64>,
    removed: Platform,
) -> std::collections::HashMap<String, Option<u64>> {
    let mut map: std::collections::HashMap<String, Option<u64>> = remaining
        .iter()
        .filter_map(|(p, c)| {
            platform_index(*p)?;
            Some((crate::platform_key(*p).to_string(), Some(*c)))
        })
        .collect();
    map.insert(crate::platform_key(removed).to_string(), None);
    map
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use ferrocast_core::event::{Millis, Origin};
    use ferrocast_core::moderation::{Capabilities, ModeChange};
    use serde_json::Value;
    use tokio::sync::watch;

    use super::*;

    struct FakeClock {
        origin: Instant,
        now_tx: watch::Sender<Instant>,
        _now_rx: watch::Receiver<Instant>,
        sleep_tx: watch::Sender<(u64, Option<Instant>)>,
        _sleep_rx: watch::Receiver<(u64, Option<Instant>)>,
        sleep_gen: AtomicUsize,
    }

    impl FakeClock {
        fn new() -> Arc<Self> {
            let t0 = Instant::now();
            let (now_tx, _now_rx) = watch::channel(t0);
            let (sleep_tx, _sleep_rx) = watch::channel((0u64, None));
            Arc::new(FakeClock { origin: t0, now_tx, _now_rx, sleep_tx, _sleep_rx, sleep_gen: AtomicUsize::new(0) })
        }

        async fn wait_sleep_after(&self, prev_gen: u64) -> (u64, Option<Instant>) {
            let mut rx = self.sleep_tx.subscribe();
            rx.wait_for(|&(g, _)| g > prev_gen).await.map(|v| *v).unwrap_or_default()
        }

        fn advance(&self, d: Duration) {
            self.now_tx.send_modify(|t| *t += d);
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            *self.now_tx.borrow()
        }

        fn wait_until(&self, deadline: Option<Instant>) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            Box::pin(async move {
                let g = self.sleep_gen.fetch_add(1, Ordering::SeqCst) as u64 + 1;
                let _ = self.sleep_tx.send((g, deadline));
                let Some(dl) = deadline else {
                    std::future::pending::<()>().await;
                    return;
                };
                let mut rx = self.now_tx.subscribe();
                let _ = rx.wait_for(|&now| now >= dl).await;
            })
        }
    }

    #[derive(Debug)]
    struct MockViewerConnector {
        platform: Platform,
        calls: AtomicUsize,
        script: Mutex<VecDeque<Result<Option<u64>, ConnectorError>>>,
        fallback: Mutex<Result<Option<u64>, ConnectorError>>,
    }

    impl MockViewerConnector {
        fn new(platform: Platform, fallback: Result<Option<u64>, ConnectorError>) -> Arc<Self> {
            Arc::new(MockViewerConnector {
                platform,
                calls: AtomicUsize::new(0),
                script: Mutex::new(VecDeque::new()),
                fallback: Mutex::new(fallback),
            })
        }

        fn push(&self, items: impl IntoIterator<Item = Result<Option<u64>, ConnectorError>>) {
            let mut q = self.script.lock().unwrap();
            for item in items {
                q.push_back(item);
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Connector for MockViewerConnector {
        fn platform(&self) -> Platform {
            self.platform
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }

        async fn send_chat(&self, _text: &str, _reply_to: Option<&str>) -> ferrocast_connector::Result<()> {
            Ok(())
        }

        async fn delete_message(&self, _message_id: &str) -> ferrocast_connector::Result<()> {
            Ok(())
        }

        async fn timeout(
            &self,
            _user_id: &str,
            _seconds: u32,
            _reason: Option<&str>,
        ) -> ferrocast_connector::Result<()> {
            Ok(())
        }

        async fn ban(&self, _user_id: &str, _reason: Option<&str>) -> ferrocast_connector::Result<()> {
            Ok(())
        }

        async fn unban(&self, _user_id: &str) -> ferrocast_connector::Result<()> {
            Ok(())
        }

        async fn set_mode(&self, _change: ModeChange) -> ferrocast_connector::Result<()> {
            Ok(())
        }

        async fn viewer_count(&self) -> ferrocast_connector::Result<Option<u64>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(next) = self.script.lock().unwrap().pop_front() {
                next
            } else {
                self.fallback.lock().unwrap().clone()
            }
        }
    }

    async fn test_engine() -> (Engine, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let eng = Engine::start_with_secrets(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default()))
            .await
            .unwrap();
        eng.stop_viewer_poller();
        (eng, dir)
    }

    #[tokio::test]
    async fn poller_waits_for_live_honors_intervals_stops_offline_and_backs_off() {
        let (eng, _d) = test_engine().await;
        let tw = MockViewerConnector::new(Platform::Twitch, Ok(Some(80)));
        let yt = MockViewerConnector::new(Platform::Youtube, Ok(Some(25)));
        eng.register_connector(tw.clone());
        eng.register_connector(yt.clone());

        let mut live_rx = eng.subscribe_live();
        let clock = FakeClock::new();
        let stop = Arc::new(Notify::new());
        let handle = tokio::spawn(run_with_clock(eng.clone(), clock.clone(), stop.clone()));

        // Initially offline: goes idle with zero polls even if clock advances.
        let (g0, dl0) = clock.wait_sleep_after(0).await;
        assert_eq!(dl0, None);
        clock.advance(Duration::from_secs(600));
        assert_eq!((tw.calls(), yt.calls()), (0, 0));

        // Twitch goes live: schedules 10 s initial delay before first poll.
        eng.ingest(Event::live(Platform::Twitch, None, Millis(1000), EventKind::StreamOnline { title: None })).await;
        let (g1, dl1) = clock.wait_sleep_after(g0).await;
        assert_eq!(dl1, Some(clock.origin + Duration::from_secs(610)));
        assert_eq!(tw.calls(), 0);

        clock.advance(Duration::from_secs(9));
        assert_eq!(tw.calls(), 0);

        // At +10s: Twitch polls once -> emits ViewerCount(42) and schedules next poll at +30s.
        tw.push([Ok(Some(42))]);
        clock.advance(Duration::from_secs(1));
        let (g2, dl2) = clock.wait_sleep_after(g1).await;
        assert_eq!(tw.calls(), 1);
        assert_eq!(yt.calls(), 0, "offline YouTube must not be polled");
        assert_eq!(dl2, Some(clock.origin + Duration::from_secs(640)));

        // Wait for pipeline task to forward ViewerCount(42) to live socket.
        let mut counts = Vec::new();
        while let Ok(frame) = live_rx.recv().await {
            let v: Value = serde_json::from_str(&frame.text).unwrap();
            if v["type"] == "event" && v["data"]["kind"]["type"] == "viewer_count" {
                counts.push((
                    v["data"]["platform"].as_str().unwrap_or("").to_string(),
                    v["data"]["kind"]["count"].as_u64().unwrap_or(0),
                ));
                break;
            }
        }
        assert_eq!(counts, vec![("twitch".into(), 42)]);

        // Transient errors trigger exponential backoff (60s -> 120s -> recovery resets to 30s).
        tw.push([
            Err(ConnectorError::Network { platform: Platform::Twitch, message: "blip 1".into() }),
            Err(ConnectorError::Network { platform: Platform::Twitch, message: "blip 2".into() }),
            Ok(Some(55)),
        ]);
        clock.advance(Duration::from_secs(30));
        let (g3, dl3) = clock.wait_sleep_after(g2).await;
        assert_eq!(tw.calls(), 2);
        assert_eq!(dl3, Some(clock.origin + Duration::from_secs(700)), "1st failure backs off to 60s");

        clock.advance(Duration::from_secs(60));
        let (g4, dl4) = clock.wait_sleep_after(g3).await;
        assert_eq!(tw.calls(), 3);
        assert_eq!(dl4, Some(clock.origin + Duration::from_secs(820)), "2nd failure backs off to 120s");

        clock.advance(Duration::from_secs(120));
        let (g5, dl5) = clock.wait_sleep_after(g4).await;
        assert_eq!(tw.calls(), 4);
        assert_eq!(dl5, Some(clock.origin + Duration::from_secs(850)), "success resets interval to 30s");

        // Twitch goes offline -> immediately stops polling.
        eng.ingest(Event::live(Platform::Twitch, None, Millis(2000), EventKind::StreamOffline)).await;
        let (g6, dl6) = clock.wait_sleep_after(g5).await;
        assert_eq!(dl6, None);
        clock.advance(Duration::from_secs(300));
        assert_eq!(tw.calls(), 4);

        // YouTube goes live -> 10s initial delay, 60s interval, quota RateLimited honors retry_after.
        yt.push([
            Ok(Some(25)),
            Err(ConnectorError::RateLimited { platform: Platform::Youtube, retry_after: Duration::from_secs(3600) }),
        ]);
        eng.ingest(Event::live(Platform::Youtube, None, Millis(3000), EventKind::StreamOnline { title: None })).await;
        let (g7, dl7) = clock.wait_sleep_after(g6).await;
        assert_eq!(dl7, Some(clock.origin + Duration::from_secs(1130)));

        clock.advance(Duration::from_secs(10));
        let (g8, dl8) = clock.wait_sleep_after(g7).await;
        assert_eq!(yt.calls(), 1);
        assert_eq!(dl8, Some(clock.origin + Duration::from_secs(1190)), "YouTube polls every 60s");

        clock.advance(Duration::from_secs(60));
        let (_g9, dl9) = clock.wait_sleep_after(g8).await;
        assert_eq!(yt.calls(), 2);
        assert_eq!(dl9, Some(clock.origin + Duration::from_secs(4790)), "RateLimited retry_after pauses 3600s");

        // Advancing 10 minutes makes zero YouTube calls while paused for quota reset.
        clock.advance(Duration::from_secs(600));
        assert_eq!(yt.calls(), 2);

        stop.notify_one();
        handle.await.unwrap();
    }

    #[test]
    fn poll_schedule_enforces_rate_limited_and_golive_youtube_interval() {
        let mut sched = PollSchedule::default();
        let t0 = Instant::now();
        sched.sync_liveness([false, true, false], [true, true, true], t0);
        assert_eq!(sched.next_deadline(), Some(t0 + INITIAL_DELAY));

        let t1 = t0 + INITIAL_DELAY;
        assert_eq!(sched.due_mask(t1), [false, true, false]);
        assert_eq!(sched.record_result(1, &Ok(Some(12)), t1), Some(12));
        assert_eq!(sched.next_deadline(), Some(t1 + POLL_YOUTUBE));

        let t2 = t1 + POLL_YOUTUBE;
        let res =
            Err(ConnectorError::RateLimited { platform: Platform::Youtube, retry_after: Duration::from_secs(7200) });
        assert_eq!(sched.record_result(1, &res, t2), None);
        assert_eq!(sched.next_deadline(), Some(t2 + Duration::from_secs(7200)));

        // Dummy check that Origin::Live isn't forgotten if reused later.
        let _ = Origin::Live;
    }

    #[test]
    fn poll_schedule_preserves_rate_limit_deadline_across_offline_restart() {
        let mut sched = PollSchedule::default();
        let t0 = Instant::now();
        sched.sync_liveness([false, true, false], [true, true, true], t0);

        let t1 = t0 + INITIAL_DELAY;
        let rl = Duration::from_secs(3600);
        let until = t1 + rl;
        let res = Err(ConnectorError::RateLimited { platform: Platform::Youtube, retry_after: rl });
        assert_eq!(sched.record_result(1, &res, t1), None);
        assert_eq!(sched.next_deadline(), Some(until));

        // Stream briefly stops and quickly goes live again before retry_after expires.
        let t2 = t1 + Duration::from_secs(5);
        sched.sync_liveness([false, false, false], [true, true, true], t2);
        let t3 = t2 + Duration::from_secs(5);
        sched.sync_liveness([false, true, false], [true, true, true], t3);
        assert_eq!(
            sched.next_deadline(),
            Some(until),
            "restarting before rate-limit expiry must retain the later retry_after deadline"
        );
    }

    #[derive(Debug)]
    struct BlockingViewerConnector {
        platform: Platform,
        result: Result<Option<u64>, ConnectorError>,
        entered: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait]
    impl Connector for BlockingViewerConnector {
        fn platform(&self) -> Platform {
            self.platform
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        async fn send_chat(&self, _text: &str, _reply_to: Option<&str>) -> ferrocast_connector::Result<()> {
            Ok(())
        }
        async fn delete_message(&self, _id: &str) -> ferrocast_connector::Result<()> {
            Ok(())
        }
        async fn timeout(
            &self,
            _user_id: &str,
            _seconds: u32,
            _reason: Option<&str>,
        ) -> ferrocast_connector::Result<()> {
            Ok(())
        }
        async fn ban(&self, _user_id: &str, _reason: Option<&str>) -> ferrocast_connector::Result<()> {
            Ok(())
        }
        async fn unban(&self, _user_id: &str) -> ferrocast_connector::Result<()> {
            Ok(())
        }
        async fn set_mode(&self, _change: ModeChange) -> ferrocast_connector::Result<()> {
            Ok(())
        }
        async fn update_stream_info(
            &self,
            _info: &ferrocast_connector::StreamInfo,
        ) -> ferrocast_connector::Result<Vec<String>> {
            Ok(vec![])
        }
        async fn viewer_count(&self) -> ferrocast_connector::Result<Option<u64>> {
            self.entered.notify_one();
            self.release.notified().await;
            self.result.clone()
        }
    }

    #[tokio::test]
    async fn run_with_clock_discards_stale_results_on_connector_swap_or_restart() {
        let (eng, _dir) = test_engine().await;
        let entered1 = Arc::new(Notify::new());
        let release1 = Arc::new(Notify::new());
        let c1 = Arc::new(BlockingViewerConnector {
            platform: Platform::Twitch,
            result: Ok(Some(11)),
            entered: entered1.clone(),
            release: release1.clone(),
        });
        eng.register_connector(c1);

        let clock = FakeClock::new();
        let stop = Arc::new(Notify::new());
        let handle = tokio::spawn(run_with_clock(eng.clone(), clock.clone(), stop.clone()));

        let (g0, _) = clock.wait_sleep_after(0).await;
        eng.ingest(Event::live(Platform::Twitch, None, Millis(1000), EventKind::StreamOnline { title: None })).await;
        let (g1, _) = clock.wait_sleep_after(g0).await;
        clock.advance(INITIAL_DELAY);
        tokio::time::timeout(Duration::from_secs(2), entered1.notified()).await.unwrap();

        // While c1 poll is in-flight, swap Twitch connector to c2.
        let entered2 = Arc::new(Notify::new());
        let release2 = Arc::new(Notify::new());
        let c2 = Arc::new(BlockingViewerConnector {
            platform: Platform::Twitch,
            result: Ok(Some(22)),
            entered: entered2.clone(),
            release: release2.clone(),
        });
        eng.register_connector(c2);
        release1.notify_one();
        let (g2, _) = clock.wait_sleep_after(g1).await;
        tokio::task::yield_now().await;
        assert_eq!(
            eng.state().viewers.get(&Platform::Twitch).copied(),
            None,
            "in-flight result from replaced connector must be ignored"
        );

        // Advance so c2 begins polling, then go offline -> back online mid-flight.
        clock.advance(POLL_TWITCH);
        tokio::time::timeout(Duration::from_secs(2), entered2.notified()).await.unwrap();
        eng.ingest(Event::live(Platform::Twitch, None, Millis(2000), EventKind::StreamOffline)).await;
        eng.ingest(Event::live(Platform::Twitch, None, Millis(2100), EventKind::StreamOnline { title: None })).await;
        release2.notify_one();
        let _ = clock.wait_sleep_after(g2).await;
        tokio::task::yield_now().await;
        assert_eq!(
            eng.state().viewers.get(&Platform::Twitch).copied(),
            None,
            "in-flight result from an ended stream session must be ignored after restart"
        );

        stop.notify_one();
        handle.await.unwrap();
    }

    #[test]
    fn golive_active_platforms_maps_twitch_youtube_and_kick_destinations() {
        let dest = |id: &str, kind: ferrocast_golive::Service| crate::golive::DestinationConfig {
            id: id.into(),
            kind,
            name: None,
            url: None,
            enabled: true,
            canvas: Default::default(),
            light: false,
            enhanced: false,
            brand: None,
            quality: crate::golive::DestinationQuality::SameAsMain,
        };
        let dests = vec![
            dest("tw-custom", ferrocast_golive::Service::Twitch),
            dest("youtube_vertical", ferrocast_golive::Service::Youtube),
            dest("kick-main", ferrocast_golive::Service::Kick),
            dest("custom-rtmp", ferrocast_golive::Service::Custom),
        ];
        let running = std::collections::BTreeSet::from(["tw-custom".into(), "kick-main".into(), "custom-rtmp".into()]);
        assert_eq!(golive_active_platforms_from(&running, &dests), [true, false, true]);

        let running_vert = std::collections::BTreeSet::from(["youtube_vertical".into(), "twitch".into()]);
        assert_eq!(golive_active_platforms_from(&running_vert, &dests), [true, true, false]);

        // Fallback ID matching when a destination isn't listed in config (e.g. youtube_vertical).
        assert_eq!(
            golive_active_platforms_from(&std::collections::BTreeSet::from(["youtube_vertical".into()]), &[]),
            [false, true, false]
        );
    }

    #[test]
    fn golive_startup_recheck_caps_deadline_when_enabled_destination_is_still_starting() {
        let dest = |id: &str, kind: ferrocast_golive::Service, enabled: bool| crate::golive::DestinationConfig {
            id: id.into(),
            kind,
            name: None,
            url: None,
            enabled,
            canvas: Default::default(),
            light: false,
            enhanced: false,
            brand: None,
            quality: crate::golive::DestinationQuality::SameAsMain,
        };
        let t0 = Instant::now();
        let next = Some(t0 + Duration::from_secs(30));
        let dests = vec![
            dest("twitch", ferrocast_golive::Service::Twitch, true),
            dest("youtube", ferrocast_golive::Service::Youtube, true),
            dest("kick", ferrocast_golive::Service::Kick, false),
        ];
        // Twitch is already active, YouTube is enabled & connected but not yet in active -> cap to 2s.
        assert_eq!(
            effective_next_deadline(next, true, [true, true, true], [true, false, false], &dests, t0),
            Some(t0 + GOLIVE_STARTUP_RECHECK)
        );
        // Once YouTube is active (Kick is disabled), keep the normal 30s deadline.
        assert_eq!(effective_next_deadline(next, true, [true, true, true], [true, true, false], &dests, t0), next);
        // After startup grace window expires for a permanently failing destination, back off to 30s.
        let past_startup = t0 + Duration::from_secs(90);
        let far_next = Some(past_startup + Duration::from_secs(60));
        assert_eq!(
            effective_next_deadline_since(
                far_next,
                true,
                [true, true, true],
                [true, false, false],
                &dests,
                Some(t0),
                past_startup
            ),
            Some(past_startup + GOLIVE_RETRY_RECHECK)
        );
    }

    #[tokio::test]
    async fn clear_stopped_golive_viewers_clears_state_overlay_and_live_without_chat_offline() {
        let (eng, _dir) = test_engine().await;
        {
            let mut st = eng.state_mut();
            st.viewers.insert(Platform::Youtube, 42);
            st.viewers.insert(Platform::Twitch, 100);
            st.live.insert(Platform::Twitch, true);
        }
        let mut live_rx = eng.subscribe_live();
        let mut overlays = eng.subscribe_overlays();

        clear_stopped_golive_viewers(&eng, [true, true, false], [true, false, false]);
        assert_eq!(eng.state().viewers.get(&Platform::Youtube).copied(), None);
        assert_eq!(eng.state().viewers.get(&Platform::Twitch).copied(), Some(100));

        let ov: Value = serde_json::from_str(&overlays.try_recv().unwrap().text).unwrap();
        assert!(ov["data"]["viewers"]["youtube"].is_null(), "overlay must clear stopped GoLive platform: {ov}");
        assert_eq!(ov["data"]["viewers"]["twitch"], 100);

        let lf: Value = serde_json::from_str(&live_rx.try_recv().unwrap().text).unwrap();
        assert_eq!(lf["type"], "status", "expected status frame on viewer clear, got: {lf}");
        assert!(lf["data"]["viewers"]["youtube"].is_null());
        assert_eq!(lf["data"]["viewers"]["twitch"], 100);
    }

    #[tokio::test]
    async fn stale_poll_on_same_connector_preserves_rate_limit_deadline() {
        let (eng, _dir) = test_engine().await;
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let conn = Arc::new(BlockingViewerConnector {
            platform: Platform::Youtube,
            result: Err(ConnectorError::RateLimited {
                platform: Platform::Youtube,
                retry_after: Duration::from_secs(3600),
            }),
            entered: entered.clone(),
            release: release.clone(),
        });
        eng.register_connector(conn);

        let clock = FakeClock::new();
        let stop = Arc::new(Notify::new());
        let handle = tokio::spawn(run_with_clock(eng.clone(), clock.clone(), stop.clone()));

        let (g0, _) = clock.wait_sleep_after(0).await;
        eng.ingest(Event::live(Platform::Youtube, None, Millis(1000), EventKind::StreamOnline { title: None })).await;
        let (g1, _) = clock.wait_sleep_after(g0).await;
        clock.advance(INITIAL_DELAY);
        tokio::time::timeout(Duration::from_secs(2), entered.notified()).await.unwrap();

        // Stream briefly restarts on the SAME connector while viewer_count() is in flight.
        eng.ingest(Event::live(Platform::Youtube, None, Millis(2000), EventKind::StreamOffline)).await;
        eng.ingest(Event::live(Platform::Youtube, None, Millis(2100), EventKind::StreamOnline { title: None })).await;
        release.notify_one();

        let (_g2, dl2) = clock.wait_sleep_after(g1).await;
        assert!(
            dl2.is_some_and(|d| d >= clock.now() + Duration::from_secs(3590)),
            "same connector RateLimited deadline must survive stale session reset: got {dl2:?} vs now {:?}",
            clock.now()
        );

        stop.notify_one();
        handle.await.unwrap();
    }
}
