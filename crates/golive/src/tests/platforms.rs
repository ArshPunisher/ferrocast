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
        retry: Retry { first: Duration::from_millis(first_ms), max: Duration::from_millis(max_ms) },
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

/// OBS drops frames when a platform falls 700 ms behind (its "drop threshold"); here a platform
/// on a slow link may be a couple of seconds behind the live picture, no more: further behind it
/// is showing the past, and the stream's delay grows by the minute. What is measured is what a
/// viewer feels: the newest frame the platform was given against the newest sent. Frames never
/// given are not delay: leaving them out is how a slow platform catches up.
#[tokio::test]
#[ignore = "finding M3.4: on a 20 KB/s link the newest picture it has is ~7 s old (frame 16 of 230, nothing dropped); agy's skip to the newest keyframe when far behind is what makes it pass"]
async fn a_slow_platform_falls_behind_alone_and_never_far() {
    const FRAME: usize = 20 * 1024;
    let w = world().await;
    let (ls, lg) = (free_listener().await, free_listener().await);
    let (sport, gport) = (ls.local_addr().unwrap().port(), lg.local_addr().unwrap().port());
    // 8 KB every 400 ms (20 KB/s) against a stream of about 600 KB/s.
    let mut slow_got = platform_that_is_slow(ls, "k", 8 * 1024, Duration::from_millis(400)).await;
    let mut good_got = platform(lg, "k").await;
    let mut slow = start(&w, destination_with(sport, "kick", 200, 4000));
    let mut good = start(&w, destination_with(gport, "twitch", 200, 4000));
    let obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    let sent = Arc::new(AtomicU64::new(0));
    let feeder = feed(obs, FRAME, Duration::from_millis(33), sent.clone());

    state_is(&mut good, |s| *s == PushState::Live).await;
    let good_since = good.borrow().since_ms;
    state_is(&mut slow, |s| *s == PushState::Live).await;
    tokio::time::sleep(Duration::from_secs(8)).await;

    // Of what was sent (30 a second), the good one has nearly all.
    let (_, videos, _) = tally(&mut good_got);
    let sent_n = sent.load(Ordering::Relaxed);
    assert!(videos as u64 * 10 >= sent_n * 8, "the good one got {videos} of {sent_n} frames");
    assert_eq!(good.borrow().state, PushState::Live);
    assert_eq!(good.borrow().since_ms, good_since, "the good one was never reconnected");
    assert_eq!(good.borrow().dropped, 0);

    // The slow one: never silent about why it isn't live, if it isn't.
    let h = slow.borrow().clone();
    match &h.state {
        PushState::Live => {}
        PushState::Reconnecting { reason, .. } => assert!(reason.contains("stalled"), "{reason}"),
        s => panic!("the slow destination is {s:?}"),
    }
    // How old is the newest picture it was given? (frame numbers ride in the frames' bytes)
    let mut newest: Option<u32> = None;
    while let Ok(g) = slow_got.try_recv() {
        if let Got::Video(_, data) = g
            && data.len() >= 9
            && data[1] == 1
        {
            let n = u32::from_be_bytes([data[5], data[6], data[7], data[8]]);
            newest = Some(newest.map_or(n, |m| m.max(n)));
        }
    }
    let newest = newest.expect("the slow platform was given pictures");
    let behind_s = (sent_n.saturating_sub(1).saturating_sub(u64::from(newest))) as f64 * 0.033;
    eprintln!(
        "slow destination: newest picture it got is frame {newest} of {sent_n} sent: {behind_s:.1} s old, dropped {}",
        h.dropped
    );
    assert!(
        behind_s <= 2.0,
        "the newest picture the slow platform has is {behind_s:.1} s old (frame {newest} of {sent_n})"
    );
    let _ = FRAME;
    feeder.abort();
}
