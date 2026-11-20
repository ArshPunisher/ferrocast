//! Unclean-exit detection and Safe Mode state management (M11-03).
//!
//! Creates `<data_dir>/running.json` on startup and removes it only on clean quit. If the marker
//! (or a leftover `studio/session.json`) is present on launch, Safe Mode starts active and offered
//! via startup status: third-party plugins and VST3s stay off, auto-start (recording/replay
//! recovery) is suppressed, and the saved scene collection is not loaded until explicitly confirmed.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{Engine, EngineError};

/// Running marker written at app launch and removed only at the end of a clean shutdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RunningMarker {
    pid: u32,
    started_at_ms: i64,
}

/// Safe Mode state exposed through `GET /api/v1/status` and `GET /api/v1/studio/engine`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafeModeStatus {
    /// Previous run ended uncleanly (running marker or studio session remained on disk).
    pub unclean_exit: bool,
    /// Safe Mode protections are currently active.
    pub active: bool,
    /// True after an unclean exit until the user decides Safe Mode vs Normal Mode.
    pub offer: bool,
    /// True while Safe Mode is waiting for explicit confirmation before loading the saved
    /// scene collection.
    pub collection_pending: bool,
    /// Third-party effect, logic, and panel plugins are suppressed.
    pub plugins_disabled: bool,
    /// Third-party VST3 plugins are suppressed.
    pub vsts_disabled: bool,
    /// Automatic start of recovery recording/replay is suppressed.
    pub auto_start_disabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Options accepted by `POST /api/v1/studio/engine/start` (empty body defaults to `Default`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StartOptions {
    /// `Some(true)` keeps/enters Safe Mode; `Some(false)` exits a crash-triggered Safe Mode offer
    /// back to Normal Mode; `None` preserves the current Safe Mode state.
    pub safe_mode: Option<bool>,
    /// When `true` in Safe Mode, confirms loading the saved scene collection.
    pub confirm_collection: bool,
}

#[derive(Default)]
pub(crate) struct SafeModeState {
    pub(crate) status: SafeModeStatus,
    pub(crate) locked_by_flag: bool,
    pub(crate) marker_dir: Option<PathBuf>,
    pub(crate) on_enable_normal: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl fmt::Debug for SafeModeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SafeModeState")
            .field("status", &self.status)
            .field("locked_by_flag", &self.locked_by_flag)
            .field("marker_dir", &self.marker_dir)
            .field("has_normal_hook", &self.on_enable_normal.is_some())
            .finish()
    }
}

pub fn running_marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join("running.json")
}

fn legacy_session_path(data_dir: &Path) -> PathBuf {
    data_dir.join("studio").join("session.json")
}

/// Checks whether a running marker or Studio crash session note remains from an unclean exit.
pub fn has_unclean_exit(data_dir: &Path) -> bool {
    let running = running_marker_path(data_dir);
    let session = legacy_session_path(data_dir);
    running.exists()
        || ferrocast_store::durable::backup_path(&running).exists()
        || session.exists()
        || ferrocast_store::durable::backup_path(&session).exists()
}

/// Records the running marker for the current process and computes the initial Safe Mode state.
pub(crate) fn begin_session_marker(data_dir: &Path, explicit_safe_mode: bool, started_at_ms: i64) -> SafeModeState {
    let unclean_exit = has_unclean_exit(data_dir);
    let marker = RunningMarker { pid: std::process::id(), started_at_ms };
    if let Err(e) = ferrocast_store::durable::save_json_pretty(&running_marker_path(data_dir), &marker) {
        tracing::warn!(error = %e, "couldn't write running marker");
    }
    let status = if explicit_safe_mode {
        SafeModeStatus {
            unclean_exit,
            active: true,
            offer: false,
            collection_pending: true,
            plugins_disabled: true,
            vsts_disabled: true,
            auto_start_disabled: true,
            reason: Some("Started in Safe Mode (--safe-mode).".into()),
        }
    } else if unclean_exit {
        SafeModeStatus {
            unclean_exit: true,
            active: true,
            offer: true,
            collection_pending: true,
            plugins_disabled: true,
            vsts_disabled: true,
            auto_start_disabled: true,
            reason: Some("Ferrocast closed unexpectedly last time.".into()),
        }
    } else {
        SafeModeStatus::default()
    };
    SafeModeState {
        status,
        locked_by_flag: explicit_safe_mode,
        marker_dir: Some(data_dir.to_path_buf()),
        on_enable_normal: None,
    }
}

/// Removes `<data_dir>/running.json` (and any `.bak`) after a clean close completes.
pub fn clear_running_marker(data_dir: &Path) {
    ferrocast_store::durable::remove_with_backup(&running_marker_path(data_dir));
}

pub(crate) fn safe_mode_refused(what: &str) -> EngineError {
    EngineError::Refused {
        status: 409,
        code: "safe_mode",
        message: format!("{what} disabled in Safe Mode."),
        hint: "Choose Normal Mode or restart Ferrocast without --safe-mode to turn plugins back on.".into(),
        retry_after: None,
    }
}

impl Engine {
    /// Current Safe Mode state for startup/status consumers.
    pub fn safe_mode_status(&self) -> SafeModeStatus {
        self.inner.safe_mode.lock().unwrap_or_else(|e| e.into_inner()).status.clone()
    }

    pub(crate) fn safe_mode_option(&self) -> Option<SafeModeStatus> {
        let s = self.safe_mode_status();
        (s.active || s.offer || s.collection_pending).then_some(s)
    }

    pub(crate) fn safe_mode_plugins_disabled(&self) -> bool {
        self.inner.safe_mode.lock().unwrap_or_else(|e| e.into_inner()).status.plugins_disabled
    }

    pub(crate) fn init_safe_mode(&self, data_dir: &Path, explicit_safe_mode: bool) {
        let next = begin_session_marker(data_dir, explicit_safe_mode, crate::now().0);
        // Only enable suppression here; leaving `false` untouched avoids clobbering concurrent tests
        // while `apply_start_options` resets it on an explicit transition back to Normal Mode.
        if next.status.vsts_disabled {
            ferrocore::afx::set_plugins_disabled(true);
        }
        let mut slot = self.inner.safe_mode.lock().unwrap_or_else(|e| e.into_inner());
        let hook = slot.on_enable_normal.take();
        *slot = SafeModeState { on_enable_normal: hook, ..next };
    }

    /// Registers a desktop hook invoked if the user exits a crash-triggered Safe Mode offer into
    /// Normal Mode (used to initialize the VST host lazily).
    pub fn on_exit_safe_mode(&self, cb: Arc<dyn Fn() + Send + Sync>) {
        let mut slot = self.inner.safe_mode.lock().unwrap_or_else(|e| e.into_inner());
        slot.on_enable_normal = Some(cb);
    }

    /// Clears the running marker on clean shutdown. Call only after Studio & outputs have closed.
    pub fn clear_running_marker(&self) {
        let dir = self.inner.safe_mode.lock().unwrap_or_else(|e| e.into_inner()).marker_dir.clone();
        if let Some(d) = dir {
            clear_running_marker(&d);
        } else {
            clear_running_marker(self.store().dir());
        }
    }

    /// Resolves Safe Mode choices when starting or confirming the Studio collection.
    /// Returns `(load_collection, allow_auto_start, enabled_normal_mode)`.
    pub(crate) fn apply_start_options(&self, opts: &StartOptions) -> Result<(bool, bool, bool), EngineError> {
        let mut enabled_normal = false;
        let mut hook: Option<Arc<dyn Fn() + Send + Sync>> = None;
        let (load_collection, allow_auto_start, vsts_disabled) = {
            let mut slot = self.inner.safe_mode.lock().unwrap_or_else(|e| e.into_inner());
            match opts.safe_mode {
                Some(false) => {
                    if slot.locked_by_flag {
                        return Err(EngineError::Refused {
                            status: 409,
                            code: "safe_mode_locked",
                            message: "Ferrocast was started with --safe-mode.".into(),
                            hint: "Restart Ferrocast normally to turn plugins and auto-start back on.".into(),
                            retry_after: None,
                        });
                    }
                    if slot.status.active {
                        enabled_normal = true;
                        hook = slot.on_enable_normal.clone();
                    }
                    slot.status.active = false;
                    slot.status.offer = false;
                    slot.status.plugins_disabled = false;
                    slot.status.vsts_disabled = false;
                    slot.status.auto_start_disabled = false;
                    slot.status.collection_pending = false;
                    slot.status.reason = None;
                }
                Some(true) => {
                    slot.status.active = true;
                    slot.status.offer = false;
                    slot.status.plugins_disabled = true;
                    slot.status.vsts_disabled = true;
                    slot.status.auto_start_disabled = true;
                    if opts.confirm_collection {
                        slot.status.collection_pending = false;
                    }
                }
                None => {
                    if opts.confirm_collection {
                        slot.status.collection_pending = false;
                        slot.status.offer = false;
                    }
                }
            }
            (!slot.status.collection_pending, !slot.status.auto_start_disabled, slot.status.vsts_disabled)
        };
        if enabled_normal {
            ferrocore::afx::set_plugins_disabled(vsts_disabled);
            if let Some(cb) = hook {
                cb();
            }
            self.load_plugins();
        }
        Ok((load_collection, allow_auto_start, enabled_normal))
    }

    #[cfg(test)]
    pub(crate) fn set_safe_mode_for_test(&self, unclean_exit: bool, explicit_flag: bool) {
        let mut slot = self.inner.safe_mode.lock().unwrap_or_else(|e| e.into_inner());
        slot.locked_by_flag = explicit_flag;
        slot.status = SafeModeStatus {
            unclean_exit,
            active: true,
            offer: unclean_exit && !explicit_flag,
            collection_pending: true,
            plugins_disabled: true,
            vsts_disabled: true,
            auto_start_disabled: true,
            reason: Some(if explicit_flag {
                "Started in Safe Mode (--safe-mode).".into()
            } else {
                "Ferrocast closed unexpectedly last time.".into()
            }),
        };
        ferrocore::afx::set_plugins_disabled(true);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use ferrocast_secrets::MemorySecrets;

    use super::*;
    use crate::studio_host::{Recovered, StudioHost, StudioSettings, StudioStatus};
    use crate::{Engine, PlatformApps};

    /// Safe Mode switches the process-wide "VST plugins off" flag: the tests that do run one at a time.
    static PLUGINS_FLAG: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[derive(Debug, Default)]
    struct MockHost {
        running: AtomicBool,
        load_collection_calls: Mutex<Vec<bool>>,
        confirm_calls: AtomicUsize,
        record_calls: AtomicUsize,
        replay_calls: AtomicUsize,
        recovered: Mutex<Option<Recovered>>,
        /// A stream is on (what stats and stream_stats then say).
        streaming: AtomicBool,
    }

    #[async_trait::async_trait]
    impl StudioHost for MockHost {
        async fn status(&self) -> StudioStatus {
            StudioStatus { available: true, running: self.running.load(Ordering::Relaxed), ..Default::default() }
        }

        async fn start(&self) -> Result<StudioStatus, String> {
            self.start_with_mode(true).await
        }

        async fn start_with_mode(&self, load_collection: bool) -> Result<StudioStatus, String> {
            self.running.store(true, Ordering::Relaxed);
            self.load_collection_calls.lock().unwrap_or_else(|e| e.into_inner()).push(load_collection);
            let recovered = self.recovered.lock().unwrap_or_else(|e| e.into_inner()).take();
            Ok(StudioStatus { available: true, running: true, recovered, ..Default::default() })
        }

        async fn confirm_collection(&self) -> Result<StudioStatus, String> {
            self.confirm_calls.fetch_add(1, Ordering::Relaxed);
            Ok(StudioStatus { available: true, running: true, ..Default::default() })
        }

        async fn stop(&self) -> Result<StudioStatus, String> {
            self.running.store(false, Ordering::Relaxed);
            Ok(StudioStatus { available: true, ..Default::default() })
        }

        async fn kinds(&self) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({}))
        }
        async fn options(&self, _kind: String) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!([]))
        }
        async fn preview(&self, _rect: Option<crate::studio_host::PreviewRect>) -> Result<(), String> {
            Ok(())
        }
        async fn defaults(&self, _kind: String) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({}))
        }
        async fn act(&self, _act: crate::studio_host::StudioAct) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!(null))
        }
        async fn settings(&self) -> StudioSettings {
            StudioSettings::default()
        }
        async fn set_settings(&self, _s: StudioSettings) -> Result<Option<String>, String> {
            Ok(None)
        }
        async fn stats(&self) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({ "stream": { "active": self.streaming.load(Ordering::Relaxed) } }))
        }
        async fn record(&self, _on: bool) -> Result<serde_json::Value, String> {
            self.record_calls.fetch_add(1, Ordering::Relaxed);
            Ok(serde_json::json!({}))
        }
        async fn record_pause(&self, _on: bool) -> Result<(), String> {
            Ok(())
        }
        async fn replay(&self, _on: bool) -> Result<serde_json::Value, String> {
            self.replay_calls.fetch_add(1, Ordering::Relaxed);
            Ok(serde_json::json!({}))
        }
        async fn monitoring(&self) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({}))
        }
        async fn stream_start(
            &self,
            _server: String,
            _key: String,
            _tracks: Vec<(u32, u32, u32)>,
        ) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({}))
        }
        async fn stream_stop(&self) -> Result<(), String> {
            Ok(())
        }
        async fn stream_stats(&self) -> Result<serde_json::Value, String> {
            if self.streaming.load(Ordering::Relaxed) {
                Ok(serde_json::json!({ "active": true, "reconnecting": false }))
            } else {
                Ok(serde_json::json!(null))
            }
        }
        async fn obs_collections(&self) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!([]))
        }
        async fn import_obs(&self, _id: String) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({}))
        }
    }

    #[test]
    fn running_marker_detects_unclean_exit_and_clears_on_clean_quit() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        assert!(!has_unclean_exit(dir.path()));

        let first = begin_session_marker(dir.path(), false, 100);
        assert!(!first.status.unclean_exit);
        assert!(!first.status.active);
        assert!(running_marker_path(dir.path()).exists());

        // Simulating a crash (running.json not cleared) triggers Safe Mode offer on next boot.
        let after_crash = begin_session_marker(dir.path(), false, 200);
        assert!(after_crash.status.unclean_exit);
        assert!(after_crash.status.active);
        assert!(after_crash.status.offer);
        assert!(after_crash.status.collection_pending);
        assert!(after_crash.status.plugins_disabled);
        assert!(after_crash.status.vsts_disabled);
        assert!(after_crash.status.auto_start_disabled);

        // Clean exit clears the running marker so subsequent start is normal.
        clear_running_marker(dir.path());
        assert!(!has_unclean_exit(dir.path()));
        let clean = begin_session_marker(dir.path(), false, 300);
        assert!(!clean.status.unclean_exit);
        assert!(!clean.status.active);
    }

    #[tokio::test]
    async fn safe_mode_defers_collection_blocks_plugins_vsts_and_autostart_until_confirmed() {
        struct ResetVsts;
        impl Drop for ResetVsts {
            fn drop(&mut self) {
                ferrocore::afx::set_plugins_disabled(false);
            }
        }
        let _reset_vsts = ResetVsts;
        let _serial = PLUGINS_FLAG.lock().await;

        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let engine = Engine::start_with(dir.path(), Arc::new(MemorySecrets::default()), PlatformApps::default())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        engine.set_safe_mode_for_test(true, false);

        let host = Arc::new(MockHost {
            recovered: Mutex::new(Some(Recovered { record: true, replay: true, live: true, ..Default::default() })),
            ..Default::default()
        });
        engine.set_studio_host(host.clone());

        // Status endpoints expose Safe Mode offer right from startup.
        let s = engine.status();
        assert_eq!(s["safe_mode"]["unclean_exit"], true);
        assert_eq!(s["safe_mode"]["active"], true);
        assert_eq!(s["safe_mode"]["collection_pending"], true);
        assert!(ferrocore::afx::plugins_disabled(), "VSTs suppressed in Safe Mode");

        // Starting Studio in Safe Mode: defers collection load and does NOT auto-start record/replay.
        let st1 = engine.studio_start().await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(*host.load_collection_calls.lock().unwrap_or_else(|e| e.into_inner()), vec![false]);
        assert_eq!(host.record_calls.load(Ordering::Relaxed), 0, "record must not auto-start in Safe Mode");
        assert_eq!(host.replay_calls.load(Ordering::Relaxed), 0, "replay must not auto-start in Safe Mode");
        assert!(st1.safe_mode.as_ref().is_some_and(|m| m.active && m.collection_pending));

        // Plugins cannot be installed or turned on while Safe Mode is active.
        assert!(engine.install_plugin(Path::new("/tmp/nonexistent.zip")).await.is_err());
        assert!(engine.set_plugin_on("test", true).await.is_err());

        // Confirming collection loads the saved collection while keeping plugins & auto-start off.
        let st2 = engine
            .studio_start_with(StartOptions { safe_mode: Some(true), confirm_collection: true })
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(host.confirm_calls.load(Ordering::Relaxed), 1);
        assert!(st2.safe_mode.as_ref().is_some_and(|m| m.active && !m.collection_pending));
        assert!(ferrocore::afx::plugins_disabled(), "VSTs stay disabled in Safe Mode after collection confirm");

        // Choosing Normal Mode re-enables plugins & VSTs and clears Safe Mode.
        let hooked = Arc::new(AtomicBool::new(false));
        let hooked_cb = hooked.clone();
        engine.on_exit_safe_mode(Arc::new(move || hooked_cb.store(true, Ordering::Relaxed)));
        let st3 = engine
            .studio_start_with(StartOptions { safe_mode: Some(false), confirm_collection: true })
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(hooked.load(Ordering::Relaxed));
        assert!(!ferrocore::afx::plugins_disabled());
        assert!(st3.safe_mode.is_none());

        // Explicit --safe-mode refuses switching back to Normal Mode without restart.
        engine.set_safe_mode_for_test(false, true);
        let err = engine
            .studio_start_with(StartOptions { safe_mode: Some(false), confirm_collection: false })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--safe-mode"));
        ferrocore::afx::set_plugins_disabled(false);
    }

    /// "Load my scenes" and "Start normally" on a Studio that is already running reload its
    /// scene collection (the undo history goes, NDI, the virtual camera and the vertical picture
    /// stop, the temporary scenes are replaced). While live that is refused like any other
    /// collection switch; with nothing held back there is nothing to load, so it isn't touched.
    #[tokio::test]
    async fn confirming_scenes_on_a_running_studio_waits_for_the_outputs_and_only_when_needed() {
        struct ResetVsts;
        impl Drop for ResetVsts {
            fn drop(&mut self) {
                ferrocore::afx::set_plugins_disabled(false);
            }
        }
        let _serial = PLUGINS_FLAG.lock().await;
        let _reset = ResetVsts;
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let engine = Engine::start_with(dir.path(), Arc::new(MemorySecrets::default()), PlatformApps::default())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        engine.set_safe_mode_for_test(true, false);
        let host = Arc::new(MockHost::default());
        engine.set_studio_host(host.clone());

        // Safe Mode, the Studio started with a blank canvas, then a stream goes live on it.
        engine.studio_start().await.unwrap_or_else(|e| panic!("{e}"));
        host.streaming.store(true, Ordering::Relaxed);
        for opts in [
            StartOptions { safe_mode: Some(true), confirm_collection: true },
            StartOptions { safe_mode: Some(false), confirm_collection: true },
        ] {
            let err = engine.studio_start_with(opts).await.unwrap_err();
            assert!(matches!(err, EngineError::Refused { status: 409, code: "studio_busy", .. }), "{err:?}");
        }
        assert_eq!(host.confirm_calls.load(Ordering::Relaxed), 0, "nothing was reloaded under the stream");
        let held = engine.safe_mode_status();
        assert!(held.active && held.offer && held.collection_pending, "a refused choice changes nothing: {held:?}");

        // Off air, the same choice goes through once.
        host.streaming.store(false, Ordering::Relaxed);
        engine
            .studio_start_with(StartOptions { safe_mode: Some(true), confirm_collection: true })
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(host.confirm_calls.load(Ordering::Relaxed), 1);

        // Nothing held back now: Start normally flips the mode, even live, and reloads nothing.
        host.streaming.store(true, Ordering::Relaxed);
        let st = engine
            .studio_start_with(StartOptions { safe_mode: Some(false), confirm_collection: true })
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(st.safe_mode.is_none());
        assert_eq!(host.confirm_calls.load(Ordering::Relaxed), 1, "no second reload with nothing pending");
        // And a plain Start on the running Studio in Normal Mode is no reload either.
        engine.studio_start().await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(host.confirm_calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn shutdown_preserves_running_marker_until_explicit_clear() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let engine = Engine::start_with(dir.path(), Arc::new(MemorySecrets::default()), PlatformApps::default())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        engine.init_safe_mode(dir.path(), false);
        assert!(has_unclean_exit(dir.path()), "running.json stays while live");

        engine.shutdown();
        assert!(has_unclean_exit(dir.path()), "WAL checkpoint shutdown alone must not clear running.json");

        engine.clear_running_marker();
        assert!(!has_unclean_exit(dir.path()), "explicit clear after clean close removes running.json");
    }
}
