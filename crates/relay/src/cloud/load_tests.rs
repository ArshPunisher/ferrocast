//! Cloud mode's load test (docs/CLOUD.md §8): N active accounts, each with its Twitch channel
//! bound and 3 overlay pages open (the Starter overlay: alerts, chat, events, labels), at a busy
//! peak of 5 events a second each (9 in 10 chat lines, 1 in 10 a follow), through the real
//! EventSub webhook (signed, mapped with the desktop's mapper, the ingest queue, the bus, the
//! alert queue with the pages' acks, chat to the chat widgets). Ignored, so it never runs in
//! CI. Run it with a release build:
//!
//! ```text
//! CLOUDLOAD_ACCOUNTS=100 CLOUDLOAD_SECS=30 cargo test -p ferrocast-relay --release --lib \
//!   -- --ignored cloudload_ --nocapture --test-threads=1
//! ```
//!
//! The driver (`cloudload_accounts`) runs this same test binary again as a child with only
//! `cloudload_child`, which serves the real `router()` on 127.0.0.1 (the cloud tests' world:
//! temporary database, fake Twitch and Kick). The child's memory and CPU are therefore the
//! relay's own, sampled with `ps`: RSS before the pages open, after they open, and after the
//! busy period, and CPU time during it. Each account's pages come from their own address
//! (`CF-Connecting-IP`), as OBS on different PCs would, so the per-IP caps don't apply.
//! `CLOUDLOAD_OUT` names a JSON file for the numbers. Nothing here talks to a real service.

use std::io::{BufRead, BufReader};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use rsa::sha2::Sha256;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::store::Channel;
use crate::bot::tests::ES_SECRET;
use crate::now_ms;

const PAGES: usize = 3;
const EVENTS_PER_SEC: u64 = 5;

fn env_num(k: &str, default: u64) -> u64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn channel_id(i: usize) -> String {
    format!("9{i:06}")
}

/// The child: a relay with N cloud mode accounts ready, printing `CLOUDREADY <json>`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "load test: the cloud driver runs it as a child"]
async fn cloudload_child() {
    if std::env::var("CLOUDLOAD_CHILD").is_err() {
        return;
    }
    let n = env_num("CLOUDLOAD_ACCOUNTS", 100) as usize;
    let w = super::tests::world().await;
    let mut tokens = Vec::with_capacity(n);
    for i in 0..n {
        let u = w.cloud_user(&format!("load{i}@example.com")).await;
        w.db.cloud_bind_channel(&Channel {
            platform: "twitch".into(),
            channel_id: channel_id(i),
            account_id: u.account.id.clone(),
            login: format!("streamer{i}"),
            name: Some(format!("Streamer {i}")),
            avatar_url: None,
            scopes: vec![],
            event_subs: vec![format!("es-{i}")],
            connected_at: now_ms(),
            error: None,
        })
        .unwrap();
        let (_, list) = w.web(&u, reqwest::Method::GET, "/c/api/v1/overlays", None).await;
        let url = list[0]["url"].as_str().unwrap();
        tokens.push(url.rsplit('/').nth(1).unwrap().to_string());
    }
    let ready = json!({ "base": w.base, "tokens": tokens, "admin": super::tests::admin_token() });
    println!("CLOUDREADY {ready}");
    // Serve until the driver kills us.
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

fn sign(msg_id: &str, ts: &str, body: &str) -> String {
    let mut m = Hmac::<Sha256>::new_from_slice(ES_SECRET.as_bytes()).unwrap();
    m.update(msg_id.as_bytes());
    m.update(ts.as_bytes());
    m.update(body.as_bytes());
    format!("sha256={}", m.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>())
}

fn rfc3339_now() -> String {
    let ms = now_ms();
    let (y, mo, d) = crate::quota::civil(ms.div_euclid(86_400_000));
    let s = ms.rem_euclid(86_400_000) / 1000;
    format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z", s / 3600, s / 60 % 60, s % 60)
}

#[derive(Default)]
struct Count {
    sent: AtomicU64,
    refused: AtomicU64,
    frames: AtomicU64,
    alerts: AtomicU64,
    closed: AtomicU64,
    failed_to_open: AtomicU64,
    /// Close codes seen (as text, for the report).
    codes: std::sync::Mutex<Vec<String>>,
}

/// One overlay page: reads every frame and acks each alert as finished (as the page does).
async fn page(base: String, token: String, ip: String, count: Arc<Count>, stop: Arc<tokio::sync::Notify>) {
    let url = format!("{}/o/{token}/ws", base.replacen("http", "ws", 1));
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert("host", "overlays.test".parse().unwrap());
    req.headers_mut().insert("cf-connecting-ip", ip.parse().unwrap());
    let ws = match tokio_tungstenite::connect_async(req).await {
        Ok((ws, _)) => ws,
        Err(e) => {
            count.failed_to_open.fetch_add(1, Ordering::Relaxed);
            count.codes.lock().unwrap().push(format!("open: {e}"));
            return;
        }
    };
    let (mut tx, mut rx) = ws.split();
    loop {
        tokio::select! {
            _ = stop.notified() => return,
            m = rx.next() => match m {
                Some(Ok(WsMsg::Text(t))) => {
                    count.frames.fetch_add(1, Ordering::Relaxed);
                    let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                    if v["type"] == "alert" {
                        count.alerts.fetch_add(1, Ordering::Relaxed);
                        let ack = json!({ "v": 1, "type": "ack", "data": { "alert_id": v["data"]["alert_id"], "phase": "finished" } });
                        let _ = tx.send(WsMsg::Text(ack.to_string().into())).await;
                    }
                }
                Some(Ok(WsMsg::Close(c))) => {
                    count.closed.fetch_add(1, Ordering::Relaxed);
                    count.codes.lock().unwrap().push(format!("close {:?}", c.map(|c| u16::from(c.code))));
                    return;
                }
                Some(Ok(_)) => {}
                other => {
                    count.closed.fetch_add(1, Ordering::Relaxed);
                    count.codes.lock().unwrap().push(format!("{other:?}"));
                    return;
                }
            },
        }
    }
}

/// One channel's busy stream: 5 events a second, signed like Twitch's.
async fn stream(http: reqwest::Client, base: String, i: usize, secs: u64, count: Arc<Count>) {
    let mut tick = tokio::time::interval(Duration::from_millis(1000 / EVENTS_PER_SEC));
    let end = Instant::now() + Duration::from_secs(secs);
    let mut n: u64 = 0;
    let b = channel_id(i);
    while Instant::now() < end {
        tick.tick().await;
        n += 1;
        let id = format!("ld-{i}-{n}");
        let (kind, event) = if n.is_multiple_of(10) {
            (
                "channel.follow",
                json!({ "user_id": format!("f{i}-{n}"), "user_login": format!("fan{n}"),
                "user_name": format!("Fan{n}"), "broadcaster_user_id": b, "followed_at": rfc3339_now() }),
            )
        } else {
            (
                "channel.chat.message",
                json!({
                "broadcaster_user_id": b, "chatter_user_id": format!("c{}", n % 40), "chatter_user_login": format!("chatter{}", n % 40),
                "chatter_user_name": format!("Chatter{}", n % 40), "message_id": id,
                "message": { "text": "gg that was a great play, !vote 2", "fragments": [] },
                "badges": [], "color": "", "message_type": "text" }),
            )
        };
        let body =
            json!({ "subscription": { "type": kind, "condition": { "broadcaster_user_id": b } }, "event": event })
                .to_string();
        let ts = rfc3339_now();
        let res = http
            .post(format!("{base}/v1/bot/twitch/eventsub"))
            .header("Twitch-Eventsub-Message-Id", &id)
            .header("Twitch-Eventsub-Message-Timestamp", &ts)
            .header("Twitch-Eventsub-Message-Signature", sign(&id, &ts, &body))
            .header("Twitch-Eventsub-Message-Type", "notification")
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await;
        match res {
            Ok(r) if r.status().is_success() => count.sent.fetch_add(1, Ordering::Relaxed),
            _ => count.refused.fetch_add(1, Ordering::Relaxed),
        };
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "load test: run with --ignored cloudload_ in a release build"]
async fn cloudload_accounts() {
    if std::env::var("CLOUDLOAD_CHILD").is_ok() {
        return;
    }
    let n = env_num("CLOUDLOAD_ACCOUNTS", 100) as usize;
    let secs = env_num("CLOUDLOAD_SECS", 30);
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "cloud::load_tests::cloudload_child", "--nocapture", "--test-threads=1"])
        .env("CLOUDLOAD_CHILD", "1")
        .env("CLOUDLOAD_ACCOUNTS", n.to_string())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let stdout = child.stdout.take().unwrap();
    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _child = Kill(child);
    let ready: Value = tokio::task::spawn_blocking(move || {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            let line = lines.next().expect("the relay child exited before it was ready").unwrap();
            if let Some((_, j)) = line.split_once("CLOUDREADY ") {
                std::thread::spawn(move || for _ in lines {});
                return serde_json::from_str(j.trim()).unwrap();
            }
        }
    })
    .await
    .unwrap();
    let base = ready["base"].as_str().unwrap().to_string();
    let tokens: Vec<String> = ready["tokens"].as_array().unwrap().iter().map(|t| t.as_str().unwrap().into()).collect();
    let admin = ready["admin"].as_str().unwrap().to_string();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (rss_idle, _) = crate::load_tests::ps(pid).unwrap();

    // Every account's pages open (3 each, from the account's own address).
    let count = Arc::new(Count::default());
    let stop = Arc::new(tokio::sync::Notify::new());
    let mut pages = tokio::task::JoinSet::new();
    for (i, t) in tokens.iter().enumerate() {
        for _ in 0..PAGES {
            let ip = format!("10.{}.{}.{}", (i >> 16) & 255, (i >> 8) & 255, i & 255);
            pages.spawn(page(base.clone(), t.clone(), ip, count.clone(), stop.clone()));
        }
        // OBS scenes don't all open in the same millisecond.
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (rss_open, cpu_open) = crate::load_tests::ps(pid).unwrap();

    // The busy peak.
    let http =
        reqwest::Client::builder().pool_max_idle_per_host(1024).timeout(Duration::from_secs(10)).build().unwrap();
    let started = Instant::now();
    let mut streams = tokio::task::JoinSet::new();
    for i in 0..n {
        streams.spawn(stream(http.clone(), base.clone(), i, secs, count.clone()));
    }
    let mut peak_rss = rss_open;
    let sampler = {
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let d2 = done.clone();
        let h = tokio::spawn(async move {
            let mut peak = 0;
            while !d2.load(Ordering::Relaxed) {
                if let Some((r, _)) = crate::load_tests::ps(pid) {
                    peak = peak.max(r);
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            peak
        });
        (done, h)
    };
    while streams.join_next().await.is_some() {}
    let wall = started.elapsed().as_secs_f64();
    tokio::time::sleep(Duration::from_secs(2)).await;
    sampler.0.store(true, Ordering::Relaxed);
    peak_rss = peak_rss.max(sampler.1.await.unwrap_or(0));
    let (rss_end, cpu_end) = crate::load_tests::ps(pid).unwrap();
    let stats: Value = http
        .get(format!("{base}/admin/v1/cloud/stats"))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    stop.notify_waiters();
    pages.abort_all();

    let cpu = cpu_end - cpu_open;
    let reasons = {
        let mut c = count.codes.lock().unwrap().clone();
        c.sort();
        c.dedup();
        c
    };
    let per_kb = |v: u64| (v.saturating_sub(rss_idle)) as f64 / n as f64;
    let out = json!({
        "accounts": n, "pages_per_account": PAGES, "events_per_sec_per_account": EVENTS_PER_SEC, "secs": wall,
        "events_sent": count.sent.load(Ordering::Relaxed), "events_refused": count.refused.load(Ordering::Relaxed),
        "frames_received": count.frames.load(Ordering::Relaxed), "alerts_received": count.alerts.load(Ordering::Relaxed),
        "pages_closed": count.closed.load(Ordering::Relaxed), "pages_failed_to_open": count.failed_to_open.load(Ordering::Relaxed),
        "close_reasons": reasons,
        "relay": { "rss_idle_kb": rss_idle, "rss_pages_open_kb": rss_open, "rss_peak_kb": peak_rss, "rss_end_kb": rss_end,
                   "cpu_seconds": cpu, "cpu_percent_of_one_core": cpu / wall * 100.0 },
        "per_account": { "kb_pages_open": per_kb(rss_open), "kb_peak": per_kb(peak_rss),
                         "cpu_percent_of_one_core": cpu / wall * 100.0 / n as f64 },
        "admin_stats": { "active_streamers": stats["active_streamers"], "overlay_sockets": stats["overlay_sockets"],
                         "events_since_start": stats["events_since_start"], "dropped_events": stats["dropped_events"] },
    });
    println!("CLOUDLOAD {}", serde_json::to_string_pretty(&out).unwrap());
    if let Ok(path) = std::env::var("CLOUDLOAD_OUT") {
        std::fs::write(path, serde_json::to_string_pretty(&out).unwrap()).unwrap();
    }
    assert_eq!(count.refused.load(Ordering::Relaxed), 0, "every webhook answered 2xx");
    assert_eq!(count.closed.load(Ordering::Relaxed), 0, "no page was closed");
    assert_eq!(count.failed_to_open.load(Ordering::Relaxed), 0, "every page opened");
    assert_eq!(stats["dropped_events"], 0, "nothing dropped");
}
