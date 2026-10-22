//! Go live everywhere in the engine: OBS setup and putting it back, a platform destination
//! that can't start saying why, and destinations started and stopped with the stream.

use std::sync::Arc;

use ferrocast_obs::ObsConfig;
use ferrocast_testkit::obs::{FakeObs, start as start_obs};
use serde_json::json;

use crate::golive::GoLiveConfig;
use crate::{Engine, PasswordChange};

async fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let e =
        Engine::start_with_secrets(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default())).await.unwrap();
    e.start_golive(0).await;
    (e, dir)
}

#[tokio::test]
async fn obs_is_pointed_here_and_put_back_exactly() {
    let (e, _dir) = engine().await;
    let st = FakeObs::studio();
    let port = start_obs(st.clone()).await;
    e.save_obs(ObsConfig { host: "127.0.0.1".into(), port }, PasswordChange::Keep).await.unwrap();
    let before = st.lock().unwrap().service.clone();
    let v = e.golive_point_obs().await.unwrap();
    assert!(v.connected && v.points_here && !v.streaming);
    assert_eq!(v.before.as_deref(), Some("Twitch"));
    let (server, key) = e.golive_ingest().await.unwrap();
    {
        let s = st.lock().unwrap();
        assert_eq!(s.service_kind, "rtmp_custom");
        assert_eq!(s.service["server"], server.as_str());
        assert_eq!(s.service["key"], key.as_str());
    }
    // Pointing again keeps the first saved setting (not our own).
    e.golive_point_obs().await.unwrap();
    // Never while OBS streams.
    st.lock().unwrap().streaming = true;
    let err = e.golive_restore_obs().await.unwrap_err().to_string();
    assert!(err.contains("streaming"), "{err}");
    st.lock().unwrap().streaming = false;
    let v = e.golive_restore_obs().await.unwrap();
    assert!(!v.points_here && v.before.is_none());
    let s = st.lock().unwrap();
    assert_eq!((s.service_kind.as_str(), &s.service), ("rtmp_common", &before));
    assert_eq!(s.service, json!({ "service": "Twitch", "server": "auto", "key": "live_123_OldKey", "bwtest": false }));
}

#[tokio::test]
async fn a_platform_that_is_not_connected_says_why_when_the_stream_starts() {
    use ferrocast_testkit::rtmp::Obs;
    let (e, _dir) = engine().await;
    let order: Vec<(String, bool)> =
        GoLiveConfig::default().destinations.into_iter().map(|d| (d.id.clone(), d.id == "twitch")).collect();
    e.set_golive_destinations(order).await.unwrap();
    let (server, key) = e.golive_ingest().await.unwrap();
    let port: u16 = server.rsplit(':').next().unwrap().split('/').next().unwrap().parse().unwrap();
    let _obs = Obs::publish(port, "live", &key).await.unwrap();
    let mut why = None;
    for _ in 0..50 {
        let v = e.golive_view().await;
        if let Some(b) = v.destinations.iter().find(|d| d.id == "twitch").and_then(|d| d.blocked.clone()) {
            why = Some(b);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(why.as_deref(), Some("Connect Twitch in Settings, Accounts first."));
}

/// The relay builds the app's Kick sign-in link: its scope list must be the app's own.
#[test]
fn the_relays_kick_scopes_are_the_apps() {
    assert_eq!(ferrocast_relay::KICK_SCOPES, ferrocast_kick::SCOPES);
}
