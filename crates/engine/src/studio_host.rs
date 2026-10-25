//! The Studio's engine (2.1, Track B) as the dashboard reaches it: over HTTP like everything
//! else (the window has no IPC permissions). The desktop app runs libobs on its main thread
//! and registers itself here as the [`StudioHost`]; the headless server has none, so its routes
//! answer "not on this computer". The engine itself never loads libobs.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Engine, EngineError};

/// What `GET /studio/engine` shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StudioStatus {
    /// OBS's engine can be used here (found on this computer, a desktop app).
    pub available: bool,
    pub running: bool,
    /// libobs's version, while running.
    pub version: Option<String>,
    /// "metal", "opengl", "d3d11".
    pub renderer: Option<String>,
    /// Why it isn't available, in plain words with what to do.
    pub detail: Option<String>,
}

/// A box in the dashboard window, in CSS pixels from its top-left corner.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct PreviewRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl PreviewRect {
    pub fn validate(&self) -> Result<(), String> {
        let ok = |v: f64, lo: f64, hi: f64| v.is_finite() && (lo..=hi).contains(&v);
        if !(ok(self.x, -20_000.0, 20_000.0) && ok(self.y, -20_000.0, 20_000.0)) {
            return Err("rect: the box is off the window.".into());
        }
        if !(ok(self.w, 1.0, 16_384.0) && ok(self.h, 1.0, 16_384.0)) {
            return Err("rect: the box must be 1 to 16384 pixels wide and high.".into());
        }
        Ok(())
    }
}

/// The desktop app's side of the Studio (it owns libobs and the window).
#[async_trait::async_trait]
pub trait StudioHost: Send + Sync + std::fmt::Debug {
    async fn status(&self) -> StudioStatus;
    async fn start(&self) -> Result<StudioStatus, String>;
    async fn stop(&self) -> Result<StudioStatus, String>;
    /// Every kind of source, filter, transition, encoder and output (`ferrocast_studio::Kinds`).
    async fn kinds(&self) -> Result<Value, String>;
    /// A kind's options (`ferrocast_studio::OptionInfo[]`).
    async fn options(&self, kind: String) -> Result<Value, String>;
    /// Shows the preview at the box in the dashboard window, or hides it (None).
    async fn preview(&self, rect: Option<PreviewRect>) -> Result<(), String>;
}

#[derive(Debug, Default)]
pub(crate) struct Slot {
    host: std::sync::RwLock<Option<Arc<dyn StudioHost>>>,
}

const NONE: &str = "The Studio runs in the Ferrocast app on your computer. Open Ferrocast there.";

impl Engine {
    /// The desktop app registers itself once its window exists.
    pub fn set_studio_host(&self, host: Arc<dyn StudioHost>) {
        *self.inner.studio_host.host.write().unwrap_or_else(|p| p.into_inner()) = Some(host);
    }

    fn studio_host(&self) -> Result<Arc<dyn StudioHost>, EngineError> {
        self.inner.studio_host.host.read().unwrap_or_else(|p| p.into_inner()).clone().ok_or(EngineError::Refused {
            status: 422,
            code: "studio_unavailable",
            message: NONE.into(),
            hint: "The Studio needs the desktop app; this is the server or a browser.".into(),
            retry_after: None,
        })
    }

    pub async fn studio_status(&self) -> StudioStatus {
        match self.studio_host() {
            Ok(h) => h.status().await,
            Err(_) => StudioStatus { detail: Some(NONE.into()), ..Default::default() },
        }
    }

    pub async fn studio_start(&self) -> Result<StudioStatus, EngineError> {
        self.studio_host()?.start().await.map_err(failed)
    }

    pub async fn studio_stop(&self) -> Result<StudioStatus, EngineError> {
        self.studio_host()?.stop().await.map_err(failed)
    }

    pub async fn studio_kinds(&self) -> Result<Value, EngineError> {
        self.studio_host()?.kinds().await.map_err(failed)
    }

    pub async fn studio_options(&self, kind: &str) -> Result<Value, EngineError> {
        if kind.is_empty()
            || kind.len() > 128
            || !kind.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
        {
            return Err(EngineError::Invalid("kind: an OBS source or filter id, like color_source.".into()));
        }
        self.studio_host()?.options(kind.to_string()).await.map_err(failed)
    }

    pub async fn studio_preview(&self, rect: Option<PreviewRect>) -> Result<(), EngineError> {
        if let Some(r) = &rect {
            r.validate().map_err(EngineError::Invalid)?;
        }
        self.studio_host()?.preview(rect).await.map_err(failed)
    }
}

/// Hosts say "Start the Studio first." when it's stopped; anything else is a failure.
pub const STOPPED: &str = "Start the Studio first.";

fn failed(message: String) -> EngineError {
    if message == STOPPED {
        return EngineError::Refused {
            status: 409,
            code: "studio_stopped",
            message,
            hint: "Press Start in Studio, Without OBS.".into(),
            retry_after: None,
        };
    }
    EngineError::Refused {
        status: 422,
        code: "studio_failed",
        message,
        hint: "Make sure OBS 32 or newer is installed, then try again.".into(),
        retry_after: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct Fake {
        rects: std::sync::Mutex<Vec<Option<PreviewRect>>>,
    }

    #[async_trait::async_trait]
    impl StudioHost for Fake {
        async fn status(&self) -> StudioStatus {
            StudioStatus { available: true, ..Default::default() }
        }
        async fn start(&self) -> Result<StudioStatus, String> {
            Ok(StudioStatus { available: true, running: true, version: Some("32.2.2".into()), ..Default::default() })
        }
        async fn stop(&self) -> Result<StudioStatus, String> {
            Err("not running".into())
        }
        async fn kinds(&self) -> Result<Value, String> {
            Ok(serde_json::json!({ "inputs": [] }))
        }
        async fn options(&self, kind: String) -> Result<Value, String> {
            Ok(serde_json::json!([{ "key": kind }]))
        }
        async fn preview(&self, rect: Option<PreviewRect>) -> Result<(), String> {
            self.rects.lock().unwrap_or_else(|p| p.into_inner()).push(rect);
            Ok(())
        }
    }

    async fn engine() -> (Engine, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::start_with_secrets(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default()))
            .await
            .unwrap();
        (e, dir)
    }

    #[tokio::test]
    async fn without_the_desktop_app_it_says_where_it_runs() {
        let (e, _d) = engine().await;
        let s = e.studio_status().await;
        assert!(!s.available && s.detail.as_deref() == Some(NONE));
        let err = e.studio_start().await.unwrap_err();
        assert!(matches!(err, EngineError::Refused { status: 422, code: "studio_unavailable", .. }), "{err:?}");
    }

    #[tokio::test]
    async fn the_host_answers_and_bad_input_never_reaches_it() {
        let (e, _d) = engine().await;
        let fake = Arc::new(Fake::default());
        e.set_studio_host(fake.clone());
        assert!(e.studio_start().await.unwrap().running);
        assert!(matches!(e.studio_stop().await.unwrap_err(), EngineError::Refused { code: "studio_failed", .. }));
        assert!(matches!(failed(STOPPED.into()), EngineError::Refused { status: 409, code: "studio_stopped", .. }));
        assert_eq!(e.studio_options("color_source").await.unwrap()[0]["key"], "color_source");
        for bad in ["", "a b", "../x", &"x".repeat(200)] {
            assert!(matches!(e.studio_options(bad).await.unwrap_err(), EngineError::Invalid(_)), "{bad}");
        }
        let r = PreviewRect { x: 10.0, y: 20.0, w: 640.0, h: 360.0 };
        e.studio_preview(Some(r)).await.unwrap();
        e.studio_preview(None).await.unwrap();
        for bad in [
            PreviewRect { w: 0.0, ..r },
            PreviewRect { h: f64::NAN, ..r },
            PreviewRect { x: f64::INFINITY, ..r },
            PreviewRect { w: 99_999.0, ..r },
        ] {
            assert!(matches!(e.studio_preview(Some(bad)).await.unwrap_err(), EngineError::Invalid(_)));
        }
        assert_eq!(*fake.rects.lock().unwrap(), vec![Some(r), None], "only good boxes reach the app");
    }
}
