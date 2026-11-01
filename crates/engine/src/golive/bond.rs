//! Ferrocast Bond (Pro) from this side: with it on, the ticked RTMP platforms of the main
//! picture are sent by the Bond service instead of from here. This computer uploads one
//! stream, over every connection at once (SRTLA), and the service sends it on; if this
//! computer drops, Be right back holds the channels for up to ten minutes. Anything that
//! goes wrong starting it sends from here as usual, and says why.

use std::collections::HashMap;
use std::time::Duration;

use ferrocast_golive::direct::{Direct, Kind};
use ferrocast_golive::{Health, PushState};
use serde_json::{Value, json};
use tokio::sync::watch;

use super::{Canvas, DestinationConfig, Running, direct_kind, lock};
use crate::{Engine, EngineError};

/// How often the service is asked how the platforms are doing, and how long it may not
/// answer before the platforms' lines say so.
const POLL: Duration = Duration::from_secs(2);
const SILENT: Duration = Duration::from_secs(15);
/// The id the bond's own send has (not a platform).
const LINK: &str = "ferrocast-bond";

/// A Bond stream running: stops its link and ends the service's session.
#[derive(Debug)]
pub(crate) struct BondRun {
    pub stop: watch::Sender<bool>,
}

fn waiting() -> Health {
    Health { state: PushState::Waiting, kbps: 0, dropped: 0, sent: 0, link: None, via: None, since_ms: 0 }
}

/// A platform's health as the service tells it, and what the stream's input is doing.
pub(crate) fn health_of(v: &Value, input: &str, since: u64) -> Health {
    let kbps = v["kbps"].as_u64().unwrap_or(0) as u32;
    let state = match (input, v["state"].as_str().unwrap_or("waiting")) {
        // Be right back is on the platform while this computer is away.
        ("away", "live") => PushState::Reconnecting {
            attempt: 0,
            retry_in_ms: 0,
            reason: "Your stream didn't reach Ferrocast Bond: Be right back is on the channel until it does (up to 10 minutes).".into(),
        },
        (_, "live") => PushState::Live,
        (_, "connecting") => PushState::Connecting,
        (_, "reconnecting") => PushState::Reconnecting {
            attempt: v["attempt"].as_u64().unwrap_or(1) as u32,
            retry_in_ms: v["retry_in_ms"].as_u64().unwrap_or(0),
            reason: v["reason"].as_str().unwrap_or("The platform dropped the stream.").to_string(),
        },
        (_, "stopped") => PushState::Stopped,
        _ => PushState::Waiting,
    };
    Health { state, kbps, dropped: v["dropped"].as_u64().unwrap_or(0), sent: 0, link: None, via: None, since_ms: since }
}

/// Words for a service answer that isn't a success.
fn refusal(v: &Value, status: u16) -> String {
    v["error"]["message"].as_str().map_or_else(|| format!("Ferrocast Bond answered {status}."), String::from)
}

impl Engine {
    /// Before the usual start pass: the platforms Bond takes now (they're put in `running`).
    /// None when it's off, already sending, or couldn't start for this stream.
    pub(crate) async fn bond_reconcile(&self, want: &[DestinationConfig], main_on: bool) {
        let g = self.golive();
        if !main_on {
            g.bond_failed.store(false, std::sync::atomic::Ordering::Relaxed);
            // The stream stopped: the link stops and the session ends.
            if let Some(r) = lock(&g.bond).take() {
                r.stop.send_replace(true);
            }
            return;
        }
        let cfg = self.golive_config();
        if !cfg.bond || g.bond_failed.load(std::sync::atomic::Ordering::Relaxed) || lock(&g.bond).is_some() {
            return;
        }
        let candidates: Vec<&DestinationConfig> = want
            .iter()
            .filter(|d| d.canvas == Canvas::Main && d.url.as_deref().and_then(direct_kind).is_none())
            .filter(|d| !self.wants_light_now(d))
            .filter(|d| !lock(&g.running).contains_key(&d.id) && !lock(&g.paused).contains_key(&d.id))
            .collect();
        if candidates.is_empty() {
            return;
        }
        let mut dests = Vec::new();
        for d in &candidates {
            // One that can't be resolved starts from here and says why.
            if let Ok(dest) = self.resolve(d).await {
                dests.push(dest);
            }
        }
        if dests.is_empty() {
            return;
        }
        match self.bond_start(&dests).await {
            Ok((run, platforms)) => {
                let mut running = lock(&g.running);
                for (id, (stop, health)) in platforms {
                    running.insert(id, Running { stop, health, light: false });
                }
                *lock(&g.bond) = Some(run);
                tracing::info!(platforms = dests.len(), "Go live: Ferrocast Bond sends this stream");
            }
            Err(e) => {
                g.bond_failed.store(true, std::sync::atomic::Ordering::Relaxed);
                let message = format!("Ferrocast Bond couldn't start ({e}): streaming from this computer.");
                tracing::warn!("{message}");
                self.studio_frame(
                    "live_alert",
                    &ferrocast_core::show::LiveAlert { id: "bond".into(), level: "warn", message, cleared: false },
                );
            }
        }
    }

    /// A Bond session for `dests`: the token from the relay, the session from the service, the
    /// link that sends there, and a watch per platform.
    #[allow(clippy::type_complexity)]
    async fn bond_start(
        &self,
        dests: &[ferrocast_golive::Destination],
    ) -> Result<(BondRun, Vec<(String, (watch::Sender<bool>, watch::Receiver<Health>))>), String> {
        let g = self.golive();
        let t = self
            .pro_call(reqwest::Method::POST, "/v1/pro/bond/token", Some(json!({})))
            .await
            .map_err(|e: EngineError| e.to_string())?;
        let (Some(token), Some(api), Some(host), Some(port)) = (
            t["token"].as_str().map(String::from),
            t["api"].as_str().map(String::from),
            t["srtla_host"].as_str().map(String::from),
            t["srtla_port"].as_u64(),
        ) else {
            return Err("the relay's answer was incomplete".into());
        };
        // Stream keys only ever go there encrypted (or to this computer, testing).
        let local = ["http://127.0.0.1", "http://localhost", "http://[::1]"]
            .iter()
            .any(|p| api.strip_prefix(p).is_some_and(|r| r.is_empty() || r.starts_with([':', '/'])));
        if !api.starts_with("https://") && !local {
            return Err("its address isn't https".into());
        }
        let stats = g.hub.stats();
        let (w, h) = (stats.width.unwrap_or(1920), stats.height.unwrap_or(1080));
        let fps = stats.fps.map_or(30, |f| f.round() as u32).clamp(1, 120);
        let body = json!({
            "token": token, "width": w, "height": h, "fps": fps,
            "destinations": dests.iter().map(|d| json!({ "id": d.id, "url": d.target.tc_url, "key": d.key })).collect::<Vec<_>>(),
        });
        let http = self.relay_http();
        let res = http
            .post(format!("{api}/v1/sessions"))
            .json(&body)
            .send()
            .await
            .map_err(|_| "it didn't answer".to_string())?;
        let status = res.status().as_u16();
        let v: Value = res.json().await.unwrap_or(Value::Null);
        if status != 201 {
            return Err(refusal(&v, status));
        }
        let (Some(id), Some(stream_id)) =
            (v["id"].as_str().map(String::from), v["stream_id"].as_str().map(String::from))
        else {
            return Err("its answer was incomplete".into());
        };
        // This computer → the service: SRTLA over every connection.
        let link = Direct {
            id: LINK.into(),
            kind: Kind::Srt,
            url: format!("srtla://{host}:{port}"),
            key: stream_id,
            retry: Default::default(),
            layer: None,
        };
        ferrocast_golive::direct::check(&link).map_err(|e| e.to_string())?;
        let (link_health, link_rx) = watch::channel(waiting());
        let (stop, stop_rx) = watch::channel(false);
        tokio::spawn(ferrocast_golive::direct::run(link, g.hub.clone(), link_health, stop_rx.clone()));
        let mut platforms = Vec::new();
        let mut watches = HashMap::new();
        for d in dests {
            let (htx, hrx) = watch::channel(waiting());
            let (s, s_rx) = watch::channel(false);
            watches.insert(d.id.clone(), (htx, s_rx));
            platforms.push((d.id.clone(), (s, hrx)));
        }
        tokio::spawn(watch_bond(http, api, token, id, watches, stop.clone(), stop_rx, link_rx));
        Ok((BondRun { stop }, platforms))
    }
}

/// Reads how the platforms are doing until every one is stopped here (or the stream ends),
/// then ends the session and the link.
#[allow(clippy::too_many_arguments)]
async fn watch_bond(
    http: reqwest::Client,
    api: String,
    token: String,
    id: String,
    mut platforms: HashMap<String, (watch::Sender<Health>, watch::Receiver<bool>)>,
    stop: watch::Sender<bool>,
    mut stopped: watch::Receiver<bool>,
    link: watch::Receiver<Health>,
) {
    let since = u64::try_from(crate::now().0).unwrap_or(0);
    let mut tick = tokio::time::interval(POLL);
    let mut heard = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = stopped.changed() => {}
        }
        // A platform unticked (or the whole stream stopped) here stops being watched.
        platforms.retain(|_, (_, s)| !*s.borrow());
        if platforms.is_empty() || *stopped.borrow() {
            break;
        }
        let res = http.get(format!("{api}/v1/sessions/{id}")).bearer_auth(&token).send().await;
        let answer = match res {
            Ok(r) if r.status().is_success() => r.json::<Value>().await.ok(),
            _ => None,
        };
        let Some(v) = answer else {
            // Silent too long: the lines say so rather than staying on their last word.
            if heard.elapsed() > SILENT {
                for (tx, _) in platforms.values() {
                    tx.send_modify(|h| {
                        h.state = PushState::Reconnecting {
                            attempt: 0,
                            retry_in_ms: POLL.as_millis() as u64,
                            reason: "Ferrocast Bond doesn't answer: it may be restarting.".into(),
                        };
                    });
                }
            }
            continue;
        };
        heard = tokio::time::Instant::now();
        let input = v["input"].as_str().unwrap_or("waiting");
        for d in v["destinations"].as_array().into_iter().flatten() {
            if let Some((tx, _)) = d["id"].as_str().and_then(|i| platforms.get(i)) {
                let mut h = health_of(&d["health"], input, since);
                // The link from here, its network read, shows on each.
                h.link = link.borrow().link;
                tx.send_replace(h);
            }
        }
        if input == "ended" {
            break;
        }
    }
    let _ = http.delete(format!("{api}/v1/sessions/{id}")).bearer_auth(&token).send().await;
    stop.send_replace(true);
    for (tx, _) in platforms.values() {
        tx.send_modify(|h| h.state = PushState::Stopped);
    }
    tracing::info!("Go live: the Ferrocast Bond stream ended");
}

#[cfg(test)]
mod tests;
