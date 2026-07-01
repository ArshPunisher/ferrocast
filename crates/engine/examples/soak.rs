//! Soak test (R-40): runs a real engine under a steady mix of chat, alerts, tips and viewer
//! counts, acks alerts like an overlay would, and fails if memory keeps growing.
//!
//! ```text
//! cargo run --release -p ferrocast-engine --example soak -- --minutes 1440 --rate 1
//! ```
//!
//! CI runs it for 5.5 hours at 5 events a second (GitHub's hosted runners stop a job at
//! 6 hours), which is more events than a day at one a second.

// A test harness: failing loudly on setup errors is the point.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use ferrocast_core::event::{Actor, Content, Event, EventKind, Millis, Platform};
use ferrocast_core::queue::AckPhase;
use ferrocast_engine::Engine;

/// How long the queue may take to empty once input stops (its cap is a few hundred alerts).
const DRAIN_LIMIT: Duration = Duration::from_secs(15 * 60);

struct Args {
    minutes: u64,
    rate: u64,
    /// Allowed growth from the first quarter's average to the last quarter's, in MB.
    max_growth_mb: f64,
}

fn args() -> Args {
    let mut a = Args { minutes: 10, rate: 5, max_growth_mb: 64.0 };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = it.next().and_then(|v| v.parse::<f64>().ok());
        match (flag.as_str(), value) {
            ("--minutes", Some(v)) => a.minutes = v as u64,
            ("--rate", Some(v)) => a.rate = (v as u64).max(1),
            ("--max-growth-mb", Some(v)) => a.max_growth_mb = v,
            _ => {
                eprintln!("usage: soak [--minutes N] [--rate EVENTS_PER_SECOND] [--max-growth-mb MB]");
                std::process::exit(2);
            }
        }
    }
    a
}

/// Resident memory of this process in MB, or `None` where we can't read it.
fn rss_mb() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let kb: f64 = status.lines().find(|l| l.starts_with("VmRSS:"))?.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb / 1024.0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()?;
        let kb: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        Some(kb / 1024.0)
    }
}

fn chat(n: u64) -> Event {
    let platform = [Platform::Twitch, Platform::Youtube, Platform::Kick][(n % 3) as usize];
    let user = n % 5_000; // a big but bounded audience, like a real channel
    let mut actor = Actor::named(platform, &format!("viewer{user}"));
    actor.id = Some(format!("u{user}"));
    let text = match n % 7 {
        0 => "!discord".to_string(),
        1 => "check out cheap-viewz.com".to_string(),
        _ => format!("message number {n} with some words in it"),
    };
    Event::live(
        platform,
        Some(format!("m{n}")),
        Millis(0),
        EventKind::ChatMessage { actor, content: Content::plain(&text), reply_to: None, is_action: false },
    )
}

fn next_event(n: u64) -> Event {
    match n % 20 {
        0 => {
            let mut e = ferrocast_core::sample::any_sample("follow", Millis(0)).expect("follow sample");
            e.origin = ferrocast_core::event::Origin::Live;
            e.source_id = Some(format!("f{n}"));
            e
        }
        5 => {
            let mut e = ferrocast_core::sample::any_sample("tip", Millis(0)).expect("tip sample");
            e.origin = ferrocast_core::event::Origin::Live;
            e.source_id = Some(format!("razorpay:pay_{n}"));
            e
        }
        10 => Event::live(Platform::Twitch, None, Millis(0), EventKind::ViewerCount { count: 100 + n % 400 }),
        _ => chat(n),
    }
}

#[tokio::main]
async fn main() {
    let a = args();
    let dir = tempfile::tempdir().expect("temp dir");
    let engine = Engine::start_with_secrets(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default()))
        .await
        .expect("engine starts");

    // An overlay: acks every alert so the queue keeps moving, as OBS would.
    let overlay_engine = engine.clone();
    let mut frames = engine.subscribe_overlays();
    tokio::spawn(async move {
        loop {
            let f = match frames.recv().await {
                Ok(f) => f,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            };
            let v: serde_json::Value = serde_json::from_str(&f.text).unwrap_or_default();
            if v["type"] == "alert"
                && let Some(id) = v["data"]["alert_id"].as_str()
            {
                overlay_engine.ack(id, AckPhase::Started).await;
                overlay_engine.ack(id, AckPhase::Finished).await;
            }
        }
    });
    // A dashboard that reads the live socket.
    let mut live = engine.subscribe_live();
    tokio::spawn(async move {
        while !matches!(live.recv().await, Err(tokio::sync::broadcast::error::RecvError::Closed)) {}
    });

    let total = Duration::from_secs(a.minutes * 60);
    let every = Duration::from_micros(1_000_000 / a.rate);
    let started = Instant::now();
    let mut samples: Vec<f64> = Vec::new();
    let mut next_sample = Instant::now();
    let mut n: u64 = 0;
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    while started.elapsed() < total {
        tick.tick().await;
        engine.ingest(next_event(n)).await;
        n += 1;
        if Instant::now() >= next_sample {
            if let Some(mb) = rss_mb() {
                samples.push(mb);
                println!("{:>6} min  {n:>9} events  {mb:>8.1} MB", started.elapsed().as_secs() / 60);
            }
            next_sample += Duration::from_secs(60);
        }
    }

    // Input stops; the queue must drain (an overloaded queue is capped, never stuck).
    let backlog = engine.queue().await.waiting.len();
    let drain_started = Instant::now();
    let drained = loop {
        let q = engine.queue().await;
        if q.waiting.is_empty() && q.playing.is_none() {
            break true;
        }
        if drain_started.elapsed() > DRAIN_LIMIT {
            break false;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    println!("done: {n} events; {backlog} alerts were waiting, drained in {}s", drain_started.elapsed().as_secs());
    if !drained {
        eprintln!("FAIL: the alert queue didn't drain within {} s of the load stopping", DRAIN_LIMIT.as_secs());
        std::process::exit(1);
    }
    if samples.len() < 8 {
        println!("too short to judge memory ({} samples); PASS", samples.len());
        return;
    }
    let quarter = samples.len() / 4;
    let avg = |s: &[f64]| s.iter().sum::<f64>() / s.len() as f64;
    // Skip the first sample: start-up allocations aren't growth.
    let first = avg(&samples[1..=quarter]);
    let last = avg(&samples[samples.len() - quarter..]);
    let growth = last - first;
    println!("memory: first quarter {first:.1} MB, last quarter {last:.1} MB, growth {growth:.1} MB");
    if growth > a.max_growth_mb {
        eprintln!("FAIL: memory grew {growth:.1} MB (limit {} MB)", a.max_growth_mb);
        std::process::exit(1);
    }
    println!("PASS");
}
