//! The stream scheduler: plan a stream (title, category, platforms, start time), make scheduled
//! broadcasts on the platforms that have them (YouTube, Facebook), and remind once before the
//! start. It never goes live by itself: going live is the streamer pressing Go live.

use std::time::Duration;

use ferrocast_connector::{ConnectorError, StreamInfo};
use ferrocast_core::event::Platform;
use ferrocast_store::repo;
use serde::{Deserialize, Serialize};

use crate::frames::{LiveFrame, Notice, NoticeLevel};
use crate::{Engine, EngineError};

const KEY: &str = "schedule";
const MAX_PLANS: usize = 50;
const TICK: Duration = Duration::from_secs(30);
/// A plan this long past its start is dropped.
const KEEP_AFTER_START_MS: i64 = 24 * 3_600_000;
/// Past this long after its start a plan's reminder is not said any more ("planned for now" would
/// be wrong after the PC slept): it is marked done quietly.
const STALE_REMINDER_MS: i64 = 30 * 60_000;
/// A plan can start at most this far ahead (a year and a day: a date, not an overflow).
const MAX_AHEAD_MS: i64 = 366 * 24 * 3_600_000;
/// Platforms a plan can name.
const PLATFORMS: &[Platform] = &[Platform::Twitch, Platform::Youtube, Platform::Kick, Platform::Facebook];

/// One platform's part of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformBroadcast {
    pub platform: String,
    /// The platform's id for the scheduled broadcast, when one was made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Why there is none (not connected, no scheduled broadcasts there, the platform said no).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledStream {
    pub id: String,
    pub info: StreamInfo,
    pub platforms: Vec<Platform>,
    /// Unix ms.
    pub start_ms: i64,
    pub remind_minutes: u32,
    #[serde(default)]
    pub reminded: bool,
    #[serde(default)]
    pub broadcasts: Vec<PlatformBroadcast>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NewScheduled {
    pub info: StreamInfo,
    pub platforms: Vec<Platform>,
    pub start_ms: i64,
    #[serde(default = "default_remind")]
    pub remind_minutes: u32,
}

fn default_remind() -> u32 {
    10
}

fn slug(p: Platform) -> String {
    crate::platform_key(p).to_string()
}

pub(crate) async fn run(engine: Engine) {
    loop {
        tokio::time::sleep(TICK).await;
        if let Err(e) = engine.schedule_tick(crate::now().0).await {
            tracing::warn!(error = %e, "the stream schedule couldn't be checked");
        }
    }
}

impl Engine {
    pub async fn schedule_list(&self) -> Result<Vec<ScheduledStream>, EngineError> {
        let mut v: Vec<ScheduledStream> = self.db(|s| repo::settings::get_or_default(s, KEY)).await?;
        v.sort_by_key(|p| p.start_ms);
        Ok(v)
    }

    async fn schedule_save(&self, v: Vec<ScheduledStream>) -> Result<(), EngineError> {
        self.db(move |s| repo::settings::set(s, KEY, &v)).await
    }

    pub async fn schedule_create(&self, new: NewScheduled) -> Result<ScheduledStream, EngineError> {
        let at = crate::now().0;
        if new.start_ms <= at {
            return Err(EngineError::Invalid("start: pick a time in the future.".into()));
        }
        if new.start_ms > at.saturating_add(MAX_AHEAD_MS) {
            return Err(EngineError::Invalid("start: at most a year ahead.".into()));
        }
        if new.platforms.is_empty() || new.platforms.iter().any(|p| !PLATFORMS.contains(p)) {
            return Err(EngineError::Invalid(
                "platforms: pick at least one of twitch, youtube, kick, facebook.".into(),
            ));
        }
        if !(1..=1440).contains(&new.remind_minutes) {
            return Err(EngineError::Invalid("remind_minutes: 1 to 1440.".into()));
        }
        let info = new.info.normalised().map_err(EngineError::Invalid)?;
        if info.title.as_deref().is_none_or(|t| t.trim().is_empty()) {
            return Err(EngineError::Invalid("title: give the stream a title.".into()));
        }
        let mut platforms = Vec::new();
        for p in new.platforms {
            if !platforms.contains(&p) {
                platforms.push(p);
            }
        }
        // A full schedule is refused before any platform is asked: a broadcast made for a plan that
        // then can't be saved is lost (YouTube's costs 50 quota units).
        if self.schedule_list().await?.len() >= MAX_PLANS {
            return Err(EngineError::Conflict(format!(
                "You can plan up to {MAX_PLANS} streams. Delete one that's done."
            )));
        }
        // The platforms are asked outside the write lock: they may be slow.
        let mut broadcasts = Vec::new();
        for p in &platforms {
            broadcasts.push(self.schedule_on(*p, &info, new.start_ms).await);
        }
        let mut b = [0u8; 6];
        getrandom::fill(&mut b).map_err(|e| EngineError::Internal(e.to_string()))?;
        let item = ScheduledStream {
            id: format!("s_{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
            info,
            platforms,
            start_ms: new.start_ms,
            remind_minutes: new.remind_minutes,
            reminded: false,
            broadcasts,
        };
        let _w = self.config_lock().await;
        let mut all = self.schedule_list().await?;
        if all.len() >= MAX_PLANS {
            return Err(EngineError::Conflict(format!(
                "You can plan up to {MAX_PLANS} streams. Delete one that's done."
            )));
        }
        all.push(item.clone());
        self.schedule_save(all).await?;
        self.push_overlay_links_locked().await;
        Ok(item)
    }

    /// Asks one platform for a scheduled broadcast; what happens is said, never an error: the
    /// plan and its reminder stand either way.
    async fn schedule_on(&self, p: Platform, info: &StreamInfo, start_ms: i64) -> PlatformBroadcast {
        let mut out = PlatformBroadcast { platform: slug(p), id: None, url: None, note: None };
        let Some(c) = self.connector(p) else {
            out.note =
                Some(format!("Connect {p} in Settings, Accounts to schedule it there. You'll still get a reminder."));
            return out;
        };
        match c.schedule_broadcast(info, start_ms).await {
            Ok(b) => {
                out.id = Some(b.id);
                out.url = b.url;
            }
            Err(ConnectorError::NotSupported { .. }) => {
                out.note = Some(format!(
                    "{p} has no scheduled broadcasts to make here: you'll get a reminder, and go live as usual."
                ));
            }
            Err(e) => out.note = Some(format!("{p} didn't schedule it ({e}). You'll still get a reminder.")),
        }
        out
    }

    pub async fn schedule_delete(&self, id: &str) -> Result<(), EngineError> {
        let _w = self.config_lock().await;
        let mut all = self.schedule_list().await?;
        let before = all.len();
        all.retain(|p| p.id != id);
        if all.len() == before {
            return Err(EngineError::NotFound("scheduled stream"));
        }
        self.schedule_save(all).await?;
        self.push_overlay_links_locked().await;
        Ok(())
    }

    /// One pass: the reminders now due (each once), and long-past plans dropped. Returns the
    /// reminders' text. It only ever tells: nothing is started here.
    pub async fn schedule_tick(&self, at_ms: i64) -> Result<Vec<String>, EngineError> {
        let _w = self.config_lock().await;
        let mut all = self.schedule_list().await?;
        // Dropped first: a plan that is going is not reminded about on its way out.
        let n = all.len();
        all.retain(|p| at_ms < p.start_ms.saturating_add(KEEP_AFTER_START_MS));
        let mut said = Vec::new();
        // Said only once the plans are saved as reminded: a failed save sends nothing, so the next
        // tick says it (once), instead of saying it now and again on every tick after.
        let mut notices: Vec<(String, String)> = Vec::new();
        let dropped = all.len() != n;
        let mut changed = dropped;
        for p in &mut all {
            let due = p.start_ms.saturating_sub(i64::from(p.remind_minutes) * 60_000);
            if !p.reminded && at_ms >= due {
                p.reminded = true;
                changed = true;
                // Long after the start (the PC was asleep): nothing to say, only to mark done.
                if at_ms > p.start_ms.saturating_add(STALE_REMINDER_MS) {
                    continue;
                }
                let title = p.info.title.clone().unwrap_or_default();
                let msg = if at_ms >= p.start_ms {
                    format!("Time to stream: \"{title}\" was planned for now.")
                } else {
                    let mins = (p.start_ms.saturating_sub(at_ms).saturating_add(59_999)) / 60_000;
                    format!("\"{title}\" starts in {mins} minute{}.", if mins == 1 { "" } else { "s" })
                };
                notices.push((format!("schedule:{}", p.id), msg.clone()));
                said.push(msg);
            }
        }
        if changed {
            self.schedule_save(all).await?;
            if dropped {
                self.push_overlay_links_locked().await;
            }
        }
        for (key, msg) in notices {
            self.send_live(LiveFrame::new("notice", &Notice::new(NoticeLevel::Info, &msg).key(key)));
        }
        Ok(said)
    }
}
