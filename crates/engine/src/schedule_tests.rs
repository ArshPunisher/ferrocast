//! The stream scheduler: a plan saved with its time, scheduled broadcasts made where a platform
//! has them, one reminder before the start, and never a go-live of its own.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrocast_connector::{Connector, ConnectorError, ScheduledBroadcast, StreamInfo};
use ferrocast_core::event::Platform;
use ferrocast_core::moderation::Capabilities;

use crate::schedule::{NewScheduled, ScheduledStream};
use crate::{Engine, EngineError, now};

#[derive(Default, Debug)]
struct Fake {
    platform: Option<Platform>,
    fail: bool,
    made: Mutex<Vec<(String, i64)>>,
}

#[async_trait]
impl Connector for Fake {
    fn platform(&self) -> Platform {
        self.platform.unwrap_or(Platform::Youtube)
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities { modes: vec![], can_ban: false, can_timeout: false, can_delete: false }
    }
    async fn send_chat(&self, _: &str, _: Option<&str>) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn delete_message(&self, _: &str) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn timeout(&self, _: &str, _: u32, _: Option<&str>) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn ban(&self, _: &str, _: Option<&str>) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn unban(&self, _: &str) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn set_mode(&self, _: ferrocast_core::moderation::ModeChange) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn schedule_broadcast(
        &self,
        info: &StreamInfo,
        start_ms: i64,
    ) -> ferrocast_connector::Result<ScheduledBroadcast> {
        if self.fail {
            return Err(ConnectorError::Refused { platform: self.platform(), message: "no".into() });
        }
        self.made.lock().unwrap().push((info.title.clone().unwrap_or_default(), start_ms));
        Ok(ScheduledBroadcast { id: "b1".into(), url: Some("https://example.test/b1".into()) })
    }
}

async fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::start_with(
        dir.path(),
        Arc::new(ferrocast_secrets::MemorySecrets::default()),
        crate::PlatformApps::default(),
    )
    .await
    .unwrap();
    (e, dir)
}

fn input(title: &str, platforms: &[Platform], in_ms: i64) -> NewScheduled {
    NewScheduled {
        info: StreamInfo { title: Some(title.into()), ..Default::default() },
        platforms: platforms.to_vec(),
        start_ms: now().0 + in_ms,
        remind_minutes: 10,
    }
}

const HOUR: i64 = 3_600_000;

#[tokio::test]
async fn a_plan_in_the_past_or_without_a_title_or_platform_is_refused_with_the_field() {
    let (e, _d) = engine().await;
    for (bad, field) in [
        (input("x", &[Platform::Youtube], -HOUR), "start"),
        (input("", &[Platform::Youtube], HOUR), "title"),
        (input("x", &[], HOUR), "platforms"),
    ] {
        let err = e.schedule_create(bad).await.unwrap_err();
        assert!(matches!(&err, EngineError::Invalid(m) if m.starts_with(field)), "{field}: {err:?}");
    }
    assert!(e.schedule_list().await.unwrap().is_empty());
}

#[tokio::test]
async fn creating_makes_scheduled_broadcasts_where_it_can_and_says_for_the_rest() {
    let (e, _d) = engine().await;
    let yt = Arc::new(Fake::default());
    e.register_connector(yt.clone());
    let item = e
        .schedule_create(input("Friday show", &[Platform::Youtube, Platform::Twitch, Platform::Facebook], 2 * HOUR))
        .await
        .unwrap();
    assert_eq!(yt.made.lock().unwrap().len(), 1);
    assert_eq!(yt.made.lock().unwrap()[0].0, "Friday show");
    let by = |p: &str| item.broadcasts.iter().find(|b| b.platform == p).unwrap().clone();
    assert_eq!(by("youtube").id.as_deref(), Some("b1"));
    assert_eq!(by("youtube").url.as_deref(), Some("https://example.test/b1"));
    // Twitch has no scheduled broadcasts for a bot to make, Facebook isn't connected: said plainly.
    assert!(by("twitch").note.as_deref().unwrap().contains("reminder"), "{:?}", by("twitch"));
    assert!(by("facebook").note.as_deref().unwrap().contains("Connect"), "{:?}", by("facebook"));
    // It survived being saved.
    assert_eq!(e.schedule_list().await.unwrap(), vec![item]);
}

#[tokio::test]
async fn a_platform_that_refuses_does_not_lose_the_plan() {
    let (e, _d) = engine().await;
    e.register_connector(Arc::new(Fake { fail: true, ..Default::default() }));
    let item = e.schedule_create(input("Show", &[Platform::Youtube], HOUR)).await.unwrap();
    let b = &item.broadcasts[0];
    assert!(b.id.is_none() && b.note.as_deref().unwrap().contains("no"), "{b:?}");
    assert_eq!(e.schedule_list().await.unwrap().len(), 1);
}

#[tokio::test]
async fn the_reminder_comes_once_before_the_start_and_nothing_goes_live() {
    let (e, _d) = engine().await;
    let item = e.schedule_create(input("Show", &[Platform::Twitch], HOUR)).await.unwrap();
    // Too early: nothing.
    assert!(e.schedule_tick(now().0).await.unwrap().is_empty());
    // Inside the 10 minutes before: one reminder, then none again.
    let near = item.start_ms - 5 * 60_000;
    let got = e.schedule_tick(near).await.unwrap();
    assert_eq!(got.len(), 1);
    assert!(got[0].contains("Show"), "{got:?}");
    assert!(e.schedule_tick(near + 1000).await.unwrap().is_empty(), "only once");
    // Past the start it still doesn't start anything: no stream, no destination turned on.
    e.schedule_tick(item.start_ms + 1000).await.unwrap();
    assert!(!e.golive_stream_running());
    let live = e.state().live.values().any(|v| *v);
    assert!(!live);
}

#[tokio::test]
async fn deleting_removes_it_and_an_unknown_id_is_not_found() {
    let (e, _d) = engine().await;
    let item = e.schedule_create(input("Show", &[Platform::Twitch], HOUR)).await.unwrap();
    e.schedule_delete(&item.id).await.unwrap();
    assert!(e.schedule_list().await.unwrap().is_empty());
    assert!(matches!(e.schedule_delete(&item.id).await.unwrap_err(), EngineError::NotFound(_)));
}

#[tokio::test]
async fn long_past_plans_are_dropped() {
    let (e, _d) = engine().await;
    let item = e.schedule_create(input("Show", &[Platform::Twitch], HOUR)).await.unwrap();
    e.schedule_tick(item.start_ms + 3 * 24 * HOUR).await.unwrap();
    assert!(e.schedule_list().await.unwrap().is_empty());
    let _: Option<ScheduledStream> = None;
}
