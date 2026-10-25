//! Music for the screens through the real engine, against the testkit's fake OBS.

use std::sync::Arc;
use std::time::Duration;

use ferrocast_obs::ObsConfig;
use ferrocast_testkit::obs::{FakeObs, start as start_obs};
use serde_json::{Map, Value, json};

use super::*;
use crate::PasswordChange;

async fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let e =
        Engine::start_with_secrets(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default())).await.unwrap();
    (e, dir)
}

fn patch(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

/// The next `music` frame that matches, within six seconds (songs are read every three).
async fn music_frame(
    rx: &mut tokio::sync::broadcast::Receiver<crate::frames::OverlayFrame>,
    ok: impl Fn(&Value) -> bool,
) -> Value {
    tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let f = rx.recv().await.unwrap();
            let v: Value = serde_json::from_str(&f.text).unwrap();
            if v["type"] == "music" && ok(&v["data"]) {
                assert_eq!(f.audience, crate::frames::Audience::Music, "only overlays that use it");
                return v["data"].clone();
            }
        }
    })
    .await
    .expect("a music frame")
}

#[tokio::test]
async fn the_beat_comes_from_the_obs_source_picked() {
    let (e, _d) = engine().await;
    let st = FakeObs::studio();
    st.lock().unwrap().meters = vec![("Desktop Audio".into(), 0.1), ("Mic".into(), 0.9)];
    let port = start_obs(st.clone()).await;
    e.save_obs(ObsConfig { host: "127.0.0.1".into(), port }, PasswordChange::Keep).await.unwrap();
    let mut rx = e.subscribe_overlays();
    assert!(!e.music_now().playing, "nothing asked for: no music");

    e.patch_music_config(&patch(json!({ "beat_input": "Desktop Audio" }))).await.unwrap();
    let m = music_frame(&mut rx, |d| d["playing"] == true).await;
    // -20 dB from Desktop Audio, not the mic's 0.9.
    assert!((m["level"].as_f64().unwrap() - 0.6).abs() < 0.02, "{m}");
    assert!(m["level"].to_string().len() <= 4, "two places, as JSON shows it: {m}");
    assert_eq!(m["title"], Value::Null);

    // The music stops: "playing" ends a little later (quiet bars aren't the end of a song).
    st.lock().unwrap().meters = vec![("Desktop Audio".into(), 0.0)];
    let m = music_frame(&mut rx, |d| d["playing"] == false).await;
    assert_eq!(m["level"], 0.0);

    // Nothing to read from any more: quiet, and the snapshot says so too.
    e.patch_music_config(&patch(json!({ "beat_input": null }))).await.unwrap();
    assert_eq!(e.music_view().config.beat_input, None);
    assert!(!e.music_now().playing);
}

#[tokio::test]
async fn the_song_can_come_from_a_text_file() {
    let (e, d) = engine().await;
    let file = d.path().join("now-playing.txt");
    std::fs::write(&file, "Midnight City\nM83\n").unwrap();
    let mut rx = e.subscribe_overlays();
    e.patch_music_config(&patch(json!({ "song": "file", "file": file.to_string_lossy() }))).await.unwrap();
    let m = music_frame(&mut rx, |d| d["title"] == "Midnight City").await;
    assert_eq!((m["artist"].as_str(), m["playing"].as_bool()), (Some("M83"), Some(true)));
    // The other app writes the next song.
    std::fs::write(&file, "Nightcall\nKavinsky\n").unwrap();
    let m = music_frame(&mut rx, |d| d["title"] == "Nightcall").await;
    assert_eq!(m["artist"], "Kavinsky");
    // Gone: the problem is said in plain words, and nothing shows.
    std::fs::remove_file(&file).unwrap();
    music_frame(&mut rx, |d| d["title"].is_null()).await;
    let v = e.music_view();
    assert!(v.song_problem.as_deref().is_some_and(|p| p.contains("song file")), "{v:?}");
}

#[tokio::test]
async fn bad_settings_are_refused_with_the_field_named() {
    let (e, _d) = engine().await;
    for (body, field) in [
        (json!({ "song": "file" }), "file"),
        (json!({ "file": "relative.txt" }), "file"),
        (json!({ "file": "/etc/hosts" }), "file"),
        (json!({ "beat_input": "a\u{7}b" }), "beat_input"),
        (json!({ "song": "spotify" }), "song"),
    ] {
        let err = e.patch_music_config(&patch(body.clone())).await.unwrap_err();
        assert!(err.to_string().starts_with(field), "{body} -> {err}");
    }
    assert_eq!(e.music_config(), MusicConfig::default(), "nothing saved");
}

#[tokio::test]
async fn settings_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let secrets: Arc<dyn ferrocast_secrets::Secrets> = Arc::new(ferrocast_secrets::MemorySecrets::default());
    {
        let e = Engine::start_with_secrets(dir.path(), secrets.clone()).await.unwrap();
        e.patch_music_config(&patch(json!({ "beat_input": "Desktop Audio", "song": "player" }))).await.unwrap();
    }
    let e = Engine::start_with_secrets(dir.path(), secrets).await.unwrap();
    let c = e.music_config();
    assert_eq!((c.beat_input.as_deref(), c.song), (Some("Desktop Audio"), SongSource::Player));
}
