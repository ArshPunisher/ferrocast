//! Platforms that misbehave the way real ones do (19 Nov: Kick took the stream and closed it
//! at once, the app never said why and never waited longer). Each test runs a bad destination
//! beside a good one off the same stream: the bad one must back off, say why, and leave the
//! good one alone.

use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::push::Retry;

/// The longest any of these waits for a state (a stall takes the 10 s write deadline).
const LONG: Duration = Duration::from_secs(30);

async fn state_within(h: &mut watch::Receiver<Health>, within: Duration, want: impl Fn(&PushState) -> bool) -> Health {
    timeout(within, h.wait_for(|h| want(&h.state))).await.expect("state in time").expect("health open").clone()
}

/// A destination with its own retry waits (short, so growth shows in a moment).
fn destination_with(port: u16, id: &str, first_ms: u64, max_ms: u64) -> Destination {
    Destination {
        id: id.into(),
        retry: Retry { first: Duration::from_millis(first_ms), max: Duration::from_millis(max_ms), ..Retry::default() },
        ..destination(port, "k")
    }
}

/// A platform that accepts the publish and then neither reads nor closes: its socket stays
/// open, its buffers fill, and the sender's writes block (a platform stuck behind its own load
/// balancer).
async fn platform_that_stalls(listener: TcpListener, key: &'static str) -> mpsc::UnboundedReceiver<Got> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let (itx, mut irx) = mpsc::unbounded_channel();
                {
                    let serve = serve_platform(&mut sock, key, &itx);
                    let published = async {
                        while let Some(g) = irx.recv().await {
                            let done = matches!(g, Got::Publish(_));
                            let _ = tx.send(g);
                            if done {
                                break;
                            }
                        }
                    };
                    tokio::select! {
                        _ = serve => {}
                        _ = published => {}
                    }
                }
                // Reads nothing from here on, and keeps the connection.
                std::future::pending::<()>().await;
            });
        }
    });
    rx
}

/// A platform on a slow link: it publishes as usual, then takes `bytes` every `every` and no more,
/// and says what it was handed (so a test can read the newest frame it got).
async fn platform_that_is_slow(
    listener: TcpListener,
    key: &'static str,
    bytes: usize,
    every: Duration,
) -> mpsc::UnboundedReceiver<Got> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let _ = serve_slowly(&mut sock, key, &tx, bytes, every).await;
                let _ = tx.send(Got::Closed);
            });
        }
    });
    rx
}

/// `serve_platform`, but once the publish is accepted each read is at most `bytes` and waits `every`.
async fn serve_slowly(
    sock: &mut TcpStream,
    key: &str,
    tx: &mpsc::UnboundedSender<Got>,
    bytes: usize,
    every: Duration,
) -> Option<()> {
    let rest = conn::handshake(sock, PeerType::Server).await.ok()?;
    let (mut s, first) = ServerSession::new(ServerSessionConfig::new()).ok()?;
    let mut queue: Vec<ServerSessionResult> = first;
    queue.extend(s.handle_input(&rest).ok()?);
    let mut buf = vec![0u8; 65536];
    let mut published = false;
    loop {
        while !queue.is_empty() {
            for r in std::mem::take(&mut queue) {
                match r {
                    ServerSessionResult::OutboundResponse(p) => sock.write_all(&p.bytes).await.ok()?,
                    ServerSessionResult::RaisedEvent(ev) => match ev {
                        ServerSessionEvent::ConnectionRequested { request_id, .. } => {
                            queue.extend(s.accept_request(request_id).ok()?)
                        }
                        ServerSessionEvent::PublishStreamRequested { request_id, stream_key, .. } => {
                            if stream_key != key {
                                return None;
                            }
                            let _ = tx.send(Got::Publish(stream_key));
                            published = true;
                            queue.extend(s.accept_request(request_id).ok()?);
                        }
                        ServerSessionEvent::VideoDataReceived { data, timestamp, .. } => {
                            let _ = tx.send(Got::Video(timestamp.value, data));
                        }
                        _ => {}
                    },
                    ServerSessionResult::UnhandleableMessageReceived(_) => {}
                }
            }
        }
        let n = if published {
            tokio::time::sleep(every).await;
            sock.read(&mut buf[..bytes]).await.ok()?
        } else {
            sock.read(&mut buf).await.ok()?
        };
        if n == 0 {
            return None;
        }
        queue.extend(s.handle_input(&buf[..n]).ok()?);
    }
}

/// A fake OBS sending a stream: headers, then a frame of `size` bytes every `every`, a keyframe
/// every tenth. Stops when the returned handle is aborted or the ingest hangs up.
fn feed(mut obs: Obs, size: usize, every: Duration, sent: Arc<AtomicU64>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        obs.meta().await;
        obs.video(0, AVC_HEADER).await;
        obs.audio(0, AAC_HEADER).await;
        let mut ts = 1000u32;
        for n in 0u64.. {
            let mut data = vec![if n % 10 == 0 { 0x17 } else { 0x27 }, 0x01, 0, 0, 0];
            data.resize(size, 0xAB);
            // Its number, so a platform can say which frame is the newest it was given.
            if size >= 9 {
                data[5..9].copy_from_slice(&(n as u32).to_be_bytes());
            }
            let Ok(r) = obs.c.publish_video_data(Bytes::from(data), RtmpTimestamp::new(ts), false) else { return };
            if obs.send(vec![r]).await.is_err() {
                return;
            }
            sent.fetch_add(1, Ordering::Relaxed);
            ts += every.as_millis() as u32;
            tokio::time::sleep(every).await;
        }
    })
}

/// What a platform has been handed so far: (publishes, video frames, closes).
fn tally(rx: &mut mpsc::UnboundedReceiver<Got>) -> (u32, u32, u32) {
    let (mut p, mut v, mut c) = (0, 0, 0);
    while let Ok(g) = rx.try_recv() {
        match g {
            Got::Publish(_) => p += 1,
            Got::Video(..) => v += 1,
            Got::Closed => c += 1,
            _ => {}
        }
    }
    (p, v, c)
}

#[tokio::test]
async fn kick_takes_the_stream_and_closes_it_waits_longer_says_why_and_spares_the_others() {
    let w = world().await;
    let (lk, lg) = (free_listener().await, free_listener().await);
    let (kport, gport) = (lk.local_addr().unwrap().port(), lg.local_addr().unwrap().port());
    let _kick_got = platform_that_drops(lk, "k", Duration::from_millis(150)).await;
    let mut good_got = platform(lg, "k").await;
    let mut kick = start(&w, destination_with(kport, "kick", 100, 2000));
    let mut good = start(&w, destination_with(gport, "twitch", 100, 2000));
    let obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    let sent = Arc::new(AtomicU64::new(0));
    let feeder = feed(obs, 600, Duration::from_millis(33), sent.clone());

    let live = state_is(&mut good, |s| *s == PushState::Live).await;
    assert_eq!(live, PushState::Live);
    let good_since = good.borrow().since_ms;

    // Five failures in a row: the count goes on, each wait doubles (±20%) and the reason is
    // said every time, with what to do about Kick.
    let mut seen = Vec::new();
    for want in 1..=5u32 {
        let h =
            state_within(&mut kick, LONG, |s| matches!(s, PushState::Reconnecting { attempt, .. } if *attempt >= want))
                .await;
        let PushState::Reconnecting { attempt, retry_in_ms, reason } = h.state else { unreachable!() };
        assert_eq!(attempt, want, "the count goes on");
        let base = (100u64 << (want - 1)).min(2000) as f64;
        assert!(
            (base * 0.8 - 1.0..=base * 1.2 + 1.0).contains(&(retry_in_ms as f64)),
            "attempt {want} waits {retry_in_ms} ms, near {base} was expected"
        );
        assert!(reason.contains("closed the connection"), "the reason says what happened: {reason}");
        assert!(reason.contains("Allow multistreaming"), "and what to do about it: {reason}");
        seen.push((attempt, retry_in_ms));
        // Off the failed state before the next wait for it.
        state_within(&mut kick, LONG, |s| !matches!(s, PushState::Reconnecting { .. })).await;
    }
    assert!(seen.windows(2).all(|p| p[1].1 > p[0].1), "the waits grow: {seen:?}");

    // The good destination never noticed: still the one session, still live, still getting frames.
    assert_eq!(good.borrow().state, PushState::Live);
    assert_eq!(good.borrow().since_ms, good_since, "it was never reconnected");
    let (publishes, videos, closes) = tally(&mut good_got);
    assert_eq!((publishes, closes), (1, 0), "one session for the whole time");
    assert!(videos > 20, "frames kept coming: {videos} of {}", sent.load(Ordering::Relaxed));
    assert_eq!(good.borrow().dropped, 0, "nothing of it was skipped");
    feeder.abort();
}

#[tokio::test]
async fn a_platform_that_stops_reading_is_called_stalled_backs_off_and_spares_the_others() {
    let w = world().await;
    let (ls, lg) = (free_listener().await, free_listener().await);
    let (sport, gport) = (ls.local_addr().unwrap().port(), lg.local_addr().unwrap().port());
    let mut stall_got = platform_that_stalls(ls, "k").await;
    let mut good_got = platform(lg, "k").await;
    let mut stuck = start(&w, destination_with(sport, "youtube", 200, 4000));
    let mut good = start(&w, destination_with(gport, "twitch", 200, 4000));
    let obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    // 64 KB every 10 ms (6 MB/s): any socket buffer is full in a second or two.
    let sent = Arc::new(AtomicU64::new(0));
    let feeder = feed(obs, 64 * 1024, Duration::from_millis(10), sent.clone());

    state_is(&mut good, |s| *s == PushState::Live).await;
    let good_since = good.borrow().since_ms;
    state_is(&mut stuck, |s| *s == PushState::Live).await;

    // While it's stuck the good one keeps its pace: compare 3 s of frames inside the stall.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let before = tally(&mut good_got).1;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let during = tally(&mut good_got).1;
    // 100 a second were sent; the stuck destination mustn't have slowed the other.
    assert!(during >= 200, "the good destination got {during} frames in 3 s, 300 were sent (before: {before})");
    assert_eq!(good.borrow().state, PushState::Live);
    assert_eq!(good.borrow().since_ms, good_since, "the good one was never reconnected");
    assert_eq!(good.borrow().dropped, 0, "nothing of the good one was skipped");

    // The stuck one: first stall, then a second after its retry; the wait doubles.
    let mut waits = Vec::new();
    for want in 1..=2u32 {
        let h = state_within(
            &mut stuck,
            LONG,
            |s| matches!(s, PushState::Reconnecting { attempt, .. } if *attempt >= want),
        )
        .await;
        let PushState::Reconnecting { attempt, retry_in_ms, reason } = h.state.clone() else { unreachable!() };
        assert_eq!(attempt, want, "the count goes on");
        assert!(reason.contains("stalled"), "the reason says it stalled: {reason}");
        assert!(reason.contains("too slow"), "and what it may be: {reason}");
        let base = (200u64 << (want - 1)) as f64;
        assert!(
            (base * 0.8 - 1.0..=base * 1.2 + 1.0).contains(&(retry_in_ms as f64)),
            "stall {want}: waits {retry_in_ms} ms, near {base} was expected"
        );
        waits.push(retry_in_ms);
        if want == 1 {
            state_within(&mut stuck, LONG, |s| !matches!(s, PushState::Reconnecting { .. })).await;
        }
    }
    assert!(waits[1] > waits[0], "the waits grow: {waits:?}");
    let (publishes, ..) = tally(&mut stall_got);
    assert!(publishes >= 2, "it was tried again ({publishes} publishes)");
    assert_eq!(good.borrow().state, PushState::Live);
    feeder.abort();
}

/// A link far too slow for the stream (160 kbit/s against 2 to 5 Mbit/s) can't stay live at any
/// buffer size: the bytes already handed to the kernel sit in the OS buffers (OBS doesn't cap
/// SO_SNDBUF either, and a cap would hurt normal links). What must happen is what upload
/// protection does (M3.5): the destination is judged slow, paused within a bounded time as the
/// bottom one of the order (the engine puts "Paused to keep Twitch smooth; it comes back by
/// itself." on it), nothing more is sent to it, and the good destination never notices.
async fn a_hopeless_link_is_paused_and_the_good_one_never_notices(frame: usize) {
    let w = world().await;
    let (ls, lg) = (free_listener().await, free_listener().await);
    let (sport, gport) = (ls.local_addr().unwrap().port(), lg.local_addr().unwrap().port());
    // 8 KB every 400 ms (20 KB/s = 160 kbit/s).
    let _slow_got = platform_that_is_slow(ls, "k", 8 * 1024, Duration::from_millis(400)).await;
    let mut good_got = platform(lg, "k").await;
    // The slow one has its own stop, as the engine's pause stops just that destination.
    let (slow_stop, slow_stop_rx) = watch::channel(false);
    let (tx, mut slow) = watch::channel(Health {
        state: PushState::Waiting,
        kbps: 0,
        dropped: 0,
        sent: 0,
        link: None,
        via: None,
        since_ms: 0,
    });
    tokio::spawn(push::run(destination_with(sport, "kick", 200, 4000), w.hub.clone(), tx, slow_stop_rx));
    let mut good = start(&w, destination_with(gport, "twitch", 200, 4000));
    let obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    let sent = Arc::new(AtomicU64::new(0));
    let feeder = feed(obs, frame, Duration::from_millis(33), sent.clone());

    state_is(&mut good, |s| *s == PushState::Live).await;
    let good_since = good.borrow().since_ms;
    state_is(&mut slow, |s| *s == PushState::Live).await;

    // The engine's loop, once a second, with the destinations that are live: the top of the
    // order keeps going, the bottom is the one that pauses.
    let mut protector = crate::protect::Protector::default();
    let order = ["twitch", "kick"];
    let started = std::time::Instant::now();
    let mut paused_after = None;
    let mut slow_sent_at_pause = 0u64;
    for second in 0..25u64 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let (g, sl) = (good.borrow().clone(), slow.borrow().clone());
        let want = w.hub.stats().kbps;
        let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        let mut live = Vec::new();
        if g.state == PushState::Live {
            live.push(sample("twitch", &g, want, now_ms));
        }
        if sl.state == PushState::Live && paused_after.is_none() {
            live.push(sample("kick", &sl, want, now_ms));
        }
        match protector.tick(started + Duration::from_secs(second), &live, &order) {
            Some(crate::protect::Action::Pause(id)) => {
                assert_eq!(id, "kick", "only the slow one is ever paused");
                slow_sent_at_pause = sl.sent;
                paused_after = Some(second + 1);
                slow_stop.send_replace(true);
            }
            Some(other) => panic!("{other:?}"),
            None => {}
        }
        if paused_after.is_some_and(|p| second >= p + 3) {
            break;
        }
    }
    let paused_after = paused_after.expect("upload protection paused the destination that can't keep up");
    // Within a bounded time: warm-up, 3 s of rate to judge, 4 s of trouble (about 7 to 9 s). It
    // used to take the 10 s write deadline more at 2 Mbit/s, when the rate looked fine until then.
    assert!(paused_after <= 12, "it took {paused_after} s to be paused");

    // Nothing more is sent to it once paused; what it was handed overall is bounded (the OS
    // buffers and one retry's burst), not growing with the stream.
    let sent_after = slow.borrow().sent;
    assert!(sent_after <= slow_sent_at_pause + 1_000_000, "{sent_after} bytes now, {slow_sent_at_pause} at the pause");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(slow.borrow().sent, sent_after, "nothing more goes to a paused destination");
    assert!(sent_after < 4_000_000, "{sent_after} bytes handed to the slow link in all");

    // The good one never noticed: live all along, never reconnected, nothing skipped, nearly
    // every frame there.
    let (_, videos, _) = tally(&mut good_got);
    let sent_n = sent.load(Ordering::Relaxed);
    assert!(videos as u64 * 10 >= sent_n * 8, "the good one got {videos} of {sent_n} frames");
    assert_eq!(good.borrow().state, PushState::Live);
    assert_eq!(good.borrow().since_ms, good_since, "the good one was never reconnected");
    assert_eq!(good.borrow().dropped, 0);
    feeder.abort();
}

/// About 5 Mbit/s: the slow one's first second looks like a burst, then its rate collapses.
#[tokio::test]
async fn a_slow_platform_on_a_5_mbit_stream_is_paused_and_the_good_one_never_notices() {
    a_hopeless_link_is_paused_and_the_good_one_never_notices(20 * 1024).await;
}

/// About 2 Mbit/s: the slow one's measured rate stays high until the 10 s write deadline, so
/// the pause comes later (and still comes).
#[tokio::test]
async fn a_slow_platform_on_a_2_mbit_stream_is_paused_and_the_good_one_never_notices() {
    a_hopeless_link_is_paused_and_the_good_one_never_notices(8 * 1024).await;
}

/// What the engine's one-second upload-protection loop does with this destination's numbers
/// (`Engine::protect_tick`): its health becomes a sample, judged against the stream's own rate.
fn sample<'a>(id: &'a str, h: &Health, want_kbps: u32, now_ms: u64) -> crate::protect::Sample<'a> {
    crate::protect::Sample {
        id,
        kbps: h.kbps,
        want_kbps,
        dropped: h.dropped,
        live_ms: now_ms.saturating_sub(h.since_ms),
    }
}

/// A destination that stopped reading blocks its writes for up to the 10 s write deadline. Its
/// rate is reported as it happens, so within about two seconds it reads 0 while it is still
/// "Live" (the state changes only at the deadline), and upload protection can see it.
#[tokio::test]
async fn a_stalled_writer_reports_no_rate_within_a_couple_of_seconds() {
    let w = world().await;
    let l = free_listener().await;
    let dport = l.local_addr().unwrap().port();
    let _got = platform_that_stalls(l, "k").await;
    let mut dest = start(&w, destination_with(dport, "kick", 200, 4000));
    let obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    let sent = Arc::new(AtomicU64::new(0));
    let feeder = feed(obs, 20 * 1024, Duration::from_millis(33), sent.clone());
    state_is(&mut dest, |s| *s == PushState::Live).await;
    // The kernel's buffers take a few hundred kilobytes first: it shows a rate.
    let filling = timeout(Duration::from_secs(5), dest.wait_for(|h| h.kbps > 500)).await.is_ok();
    assert!(filling, "the buffers filled at some rate");
    // Then nothing is taken: 0, and still Live, within a few seconds (the deadline is 10).
    let got = timeout(Duration::from_secs(4), dest.wait_for(|h| h.kbps == 0 && h.sent > 100_000)).await.is_ok();
    let h = dest.borrow().clone();
    assert!(got, "no rate reported while its writes are blocked: {h:?}");
    assert_eq!(h.state, PushState::Live, "still within the write deadline");
    feeder.abort();
}

// --- a backup server for the same key (YouTube's backup ingest, M10-05) ---------------------

/// A destination with a backup server beside its own.
fn with_backup(primary: u16, backup: u16) -> Destination {
    Destination {
        backup: Some(Target::parse(&format!("rtmp://127.0.0.1:{backup}/app")).unwrap()),
        ..destination_with(primary, "youtube", 20, 100)
    }
}

/// A port nothing listens on.
async fn dead_port() -> u16 {
    let l = free_listener().await;
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

/// A main server that can't be reached: after `BACKUP_AFTER` tries the stream goes to the backup,
/// said in words, and it is live there.
#[tokio::test]
async fn a_dead_main_server_goes_to_the_backup_after_a_few_tries() {
    let w = world().await;
    let primary = dead_port().await;
    let l = free_listener().await;
    let bport = l.local_addr().unwrap().port();
    let mut got = platform(l, "k").await;
    let mut health = start(&w, with_backup(primary, bport));
    let _obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    // The third failure says it is switching.
    let said = state_within(
        &mut health,
        LONG,
        |s| matches!(s, PushState::Reconnecting { reason, .. } if reason.contains("backup server")),
    )
    .await;
    match said.state {
        PushState::Reconnecting { attempt, .. } => {
            assert_eq!(attempt, push::BACKUP_AFTER, "after {} tries", push::BACKUP_AFTER)
        }
        s => panic!("{s:?}"),
    }
    state_within(&mut health, LONG, |s| *s == PushState::Live).await;
    assert_eq!(next(&mut got).await, Got::Publish("k".into()), "the same key, at the backup");
}

/// A destination without a backup (every one but YouTube) is as it was: it keeps trying its one
/// server and never says anything about another.
#[tokio::test]
async fn a_destination_without_a_backup_keeps_trying_its_own_server() {
    let w = world().await;
    let primary = dead_port().await;
    let mut health = start(&w, destination_with(primary, "twitch", 20, 100));
    let _obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    for _ in 0..2 * push::BACKUP_AFTER {
        match state_within(&mut health, LONG, |s| {
            matches!(s, PushState::Reconnecting { .. } | PushState::Failed { .. })
        })
        .await
        .state
        {
            PushState::Reconnecting { reason, .. } => assert!(!reason.contains("backup"), "{reason}"),
            PushState::Failed { reason, detail, .. } => {
                assert!(!reason.contains("backup"), "{reason}");
                assert!(!detail.as_deref().unwrap_or("").contains("backup"), "{detail:?}");
                break;
            }
            s => panic!("{s:?}"),
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    let attempt = match &health.borrow().state {
        PushState::Reconnecting { attempt, .. } | PushState::Failed { attempts: attempt, .. } => *attempt,
        s => panic!("{s:?}"),
    };
    assert!(attempt > push::BACKUP_AFTER, "{attempt}");
}

/// A key the server refuses isn't a server that can't be reached: no backup is tried for it.
#[tokio::test]
async fn a_refused_key_does_not_move_to_the_backup() {
    let w = world().await;
    let (lp, lb) = (free_listener().await, free_listener().await);
    let (pport, bport) = (lp.local_addr().unwrap().port(), lb.local_addr().unwrap().port());
    let _p = platform(lp, "right-key").await;
    let mut b = platform(lb, "right-key").await;
    let dest = Destination { key: "wrong-key".into(), ..with_backup(pport, bport) };
    let mut health = start(&w, dest);
    let _obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    for _ in 0..push::BACKUP_AFTER + 2 {
        match state_within(&mut health, LONG, |s| {
            matches!(s, PushState::Reconnecting { .. } | PushState::Failed { .. })
        })
        .await
        .state
        {
            PushState::Reconnecting { reason, .. } => assert!(!reason.contains("backup"), "{reason}"),
            PushState::Failed { reason, detail, refused, .. } => {
                assert!(refused);
                assert!(!reason.contains("backup"), "{reason}");
                assert!(!detail.as_deref().unwrap_or("").contains("backup"), "{detail:?}");
                break;
            }
            s => panic!("{s:?}"),
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    assert!(timeout(Duration::from_millis(200), b.recv()).await.is_err(), "the backup was never asked");
}

/// The main server comes back: the next stream starts on it, not on the backup that held before.
#[tokio::test]
async fn the_next_stream_starts_on_the_main_server_again() {
    let w = world().await;
    let primary = dead_port().await;
    let l = free_listener().await;
    let bport = l.local_addr().unwrap().port();
    let mut backup_got = platform(l, "k").await;
    let mut health = start(&w, with_backup(primary, bport));
    let obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    state_within(&mut health, LONG, |s| *s == PushState::Live).await;
    assert_eq!(next(&mut backup_got).await, Got::Publish("k".into()));
    // The stream ends; the main server is back before the next one.
    drop(obs);
    state_within(&mut health, LONG, |s| *s == PushState::Waiting).await;
    let mut main_got = platform(TcpListener::bind(("127.0.0.1", primary)).await.unwrap(), "k").await;
    let _obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    state_within(&mut health, LONG, |s| *s == PushState::Live).await;
    assert_eq!(next(&mut main_got).await, Got::Publish("k".into()), "the main server, the backup not needed");
}

/// A backup that held for a while and then dropped: the main server first again (it may have
/// come back meanwhile), not the backup that just failed.
#[tokio::test]
async fn a_backup_that_held_then_dropped_gives_way_to_the_main_server() {
    let w = world().await;
    let primary = dead_port().await;
    let l = free_listener().await;
    let bport = l.local_addr().unwrap().port();
    // It takes the stream and hangs up after 31 s (STAYED_LIVE_MS is 30).
    let mut backup_got = platform_that_drops(l, "k", Duration::from_secs(31)).await;
    let mut health = start(&w, with_backup(primary, bport));
    // OBS keeps sending (the ingest hangs up on a silent one after 30 s).
    let mut obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    obs.meta().await;
    obs.video(0, AVC_HEADER).await;
    tokio::spawn(async move {
        let mut ts = 10;
        loop {
            obs.video(ts, if ts % 2000 == 0 { KEY } else { INTER }).await;
            ts += 1000;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    state_within(&mut health, LONG, |s| *s == PushState::Live).await;
    assert_eq!(next(&mut backup_got).await, Got::Publish("k".into()));
    // The main server is up now.
    let mut main_got = platform(TcpListener::bind(("127.0.0.1", primary)).await.unwrap(), "k").await;
    let said = state_within(
        &mut health,
        Duration::from_secs(40),
        |s| matches!(s, PushState::Reconnecting { reason, .. } if reason.contains("main server again")),
    )
    .await;
    assert!(matches!(said.state, PushState::Reconnecting { .. }));
    state_within(&mut health, LONG, |s| *s == PushState::Live).await;
    assert_eq!(next(&mut main_got).await, Got::Publish("k".into()));
}
