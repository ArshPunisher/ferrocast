//! Runs ffmpeg for the cut-from-stream vertical picture ([`crate::cut`]): while the normal stream
//! is live, its packets go to ffmpeg's stdin as FLV, and ffmpeg publishes the upright picture to
//! Ferrocast's own vertical key. An encoder that won't start is skipped for the next one; ffmpeg
//! stopping by itself is reported in words and tried again.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, watch};

use crate::cut::{self, CutConfig, Encoder};
use crate::hub::{Frame, Hub};

/// An encoder that ends this soon after starting didn't work here: the next one is tried.
const STARTUP: Duration = Duration::from_secs(4);
/// How long to wait for the stream's size before giving up on this stream.
const SIZE_WAIT: Duration = Duration::from_secs(10);
/// After ffmpeg stopped by itself, this long before trying again.
const RETRY: Duration = Duration::from_secs(3);

/// What the cutter is doing, for the dashboard.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CutState {
    /// Waiting for the normal stream.
    Waiting,
    Starting,
    /// Making the vertical picture, encoded on `on` ("your graphics card").
    Running {
        encoder: String,
        on: String,
    },
    /// It stopped; `reason` says why in words. It tries again by itself.
    Failed {
        reason: String,
    },
}

/// Runs until `stop` turns true. `encoders` are tried best first.
pub async fn run(
    ffmpeg: PathBuf,
    hub: Arc<Hub>,
    cfg: CutConfig,
    encoders: Vec<Encoder>,
    publish: String,
    state: watch::Sender<CutState>,
    mut stop: watch::Receiver<bool>,
) {
    let mut live = hub.live();
    let mut first_ok: usize = 0;
    loop {
        state.send_replace(CutState::Waiting);
        tokio::select! {
            _ = stopped(&mut stop) => return,
            on = until(&mut live, true) => if !on { return },
        }
        // ffmpeg needs the picture's size for the crop boxes.
        let mut size = None;
        let waited = tokio::time::Instant::now();
        while waited.elapsed() < SIZE_WAIT {
            let s = hub.stats();
            // A size of nothing (or a pixel) isn't one: keep waiting.
            if let (Some(w @ 16..), Some(h @ 16..)) = (s.width, s.height) {
                size = Some((w, h));
                break;
            }
            tokio::select! {
                _ = stopped(&mut stop) => return,
                _ = tokio::time::sleep(Duration::from_millis(250)) => {}
            }
        }
        let Some((w, h)) = size else {
            state.send_replace(CutState::Failed {
                reason: "OBS's stream didn't say its size, so Ferrocast can't cut it. Restart the stream in OBS."
                    .into(),
            });
            wait_for_end(&hub, &mut stop).await;
            continue;
        };
        let mut started = false;
        for (i, enc) in encoders.iter().enumerate().skip(first_ok) {
            state.send_replace(CutState::Starting);
            match run_once(&ffmpeg, &hub, &cfg, (w, h), enc, &publish, &state, &mut stop).await {
                Outcome::Stopped => return,
                Outcome::StreamEnded => {
                    first_ok = i;
                    started = true;
                    break;
                }
                Outcome::DidntStart(why) => {
                    tracing::info!(encoder = enc.name, %why, "vertical cut: encoder didn't start, trying the next");
                    if i + 1 == encoders.len() {
                        state.send_replace(CutState::Failed { reason: why });
                    }
                }
                Outcome::Died(why) => {
                    first_ok = i;
                    started = true;
                    state.send_replace(CutState::Failed { reason: why });
                    tokio::select! {
                        _ = stopped(&mut stop) => return,
                        _ = tokio::time::sleep(RETRY) => {}
                    }
                    break;
                }
            }
        }
        if !started {
            // No encoder worked: try again with the next stream.
            wait_for_end(&hub, &mut stop).await;
        }
    }
}

async fn wait_for_end(hub: &Hub, stop: &mut watch::Receiver<bool>) {
    let mut live = hub.live();
    tokio::select! {
        _ = stopped(stop) => {}
        _ = until(&mut live, false) => {}
    }
}

enum Outcome {
    Stopped,
    StreamEnded,
    DidntStart(String),
    Died(String),
}

#[allow(clippy::too_many_arguments)]
async fn run_once(
    ffmpeg: &PathBuf,
    hub: &Hub,
    cfg: &CutConfig,
    (w, h): (u32, u32),
    enc: &Encoder,
    publish: &str,
    state: &watch::Sender<CutState>,
    stop: &mut watch::Receiver<bool>,
) -> Outcome {
    let args = cut::ffmpeg_args(cfg, w, h, enc, publish);
    let mut child = match Command::new(ffmpeg)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Outcome::DidntStart(format!("ffmpeg couldn't start: {e}")),
    };
    let last_error = Arc::new(std::sync::Mutex::new(String::new()));
    if let Some(err) = child.stderr.take() {
        let last = last_error.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                if !l.trim().is_empty() {
                    *last.lock().unwrap_or_else(|p| p.into_inner()) = l.chars().take(300).collect();
                }
            }
        });
    }
    let Some(mut stdin) = child.stdin.take() else { return Outcome::DidntStart("ffmpeg has no input".into()) };
    let began = tokio::time::Instant::now();
    let sub = hub.subscribe();
    let mut running_said = false;
    // The feeder owns ffmpeg's input and closes it when the stream ends, so ffmpeg finishes.
    let feed = async move {
        let r = pump(&mut stdin, sub).await;
        let _ = stdin.shutdown().await;
        r
    };
    tokio::pin!(feed);
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            _ = stopped(stop) => {
                let _ = child.kill().await;
                return Outcome::Stopped;
            }
            ended = &mut feed => {
                match ended {
                    Ok(_) => {
                        // The stream ended: let ffmpeg finish, then wait for the next one.
                        stop_child(&mut child).await;
                        return Outcome::StreamEnded;
                    }
                    Err(_) => {
                        let _ = child.wait().await;
                        let why = said(&last_error);
                        return if began.elapsed() < STARTUP { Outcome::DidntStart(why) } else { Outcome::Died(why) };
                    }
                }
            }
            status = child.wait() => {
                let why = match status {
                    Ok(s) if s.success() => "ffmpeg stopped".to_string(),
                    _ => said(&last_error),
                };
                return if began.elapsed() < STARTUP { Outcome::DidntStart(why) } else { Outcome::Died(why) };
            }
            _ = tick.tick() => {
                if !running_said && began.elapsed() >= STARTUP {
                    running_said = true;
                    state.send_replace(CutState::Running { encoder: enc.name.into(), on: enc.on.into() });
                }
            }
        }
    }
}

/// Writes the stream as FLV: the headers, then every packet from the next keyframe with time
/// starting at zero. Falling behind skips ahead to a keyframe (nothing broken goes in).
/// `Ok` when the stream ended.
async fn pump(stdin: &mut tokio::process::ChildStdin, mut sub: crate::hub::Subscription) -> std::io::Result<()> {
    stdin.write_all(&cut::flv_header()).await?;
    if let Some(v) = &sub.headers.video {
        stdin.write_all(&cut::flv_tag(9, 0, v)).await?;
    }
    if let Some(a) = &sub.headers.audio {
        stdin.write_all(&cut::flv_tag(8, 0, a)).await?;
    }
    let mut base: Option<u32> = None;
    let mut need_key = true;
    loop {
        let frame = match sub.rx.recv().await {
            Ok(f) => f,
            Err(broadcast::error::RecvError::Lagged(_)) => {
                need_key = true;
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        };
        match frame {
            Frame::Video { ts, data, key } => {
                if need_key && !key {
                    continue;
                }
                need_key = false;
                let b = *base.get_or_insert(ts);
                stdin.write_all(&cut::flv_tag(9, ts.wrapping_sub(b), &data)).await?;
            }
            Frame::Audio { ts, data } => {
                if let Some(b) = base.filter(|b| ts >= *b) {
                    stdin.write_all(&cut::flv_tag(8, ts - b, &data)).await?;
                }
            }
            Frame::Metadata(_) => {}
            Frame::End => return Ok(()),
        }
    }
}

/// Until `stop` turns true (the watch guard isn't kept: it isn't Send).
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|s| *s).await;
}

/// Until the stream is (or isn't) live; false when the hub is gone.
async fn until(live: &mut watch::Receiver<crate::hub::Live>, on: bool) -> bool {
    live.wait_for(|l| l.on == on).await.is_ok()
}

/// ffmpeg's last words, as a sentence for the streamer.
fn said(last: &std::sync::Mutex<String>) -> String {
    let l = last.lock().unwrap_or_else(|p| p.into_inner()).clone();
    if l.is_empty() {
        "The vertical picture stopped (ffmpeg ended without saying why).".into()
    } else {
        format!("The vertical picture stopped: {l}")
    }
}

async fn stop_child(child: &mut Child) {
    drop(child.stdin.take());
    if tokio::time::timeout(Duration::from_secs(3), child.wait()).await.is_err() {
        let _ = child.kill().await;
    }
}
