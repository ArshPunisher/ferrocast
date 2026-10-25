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
    /// The settings a new source of `kind` starts with.
    async fn defaults(&self, kind: String) -> Result<Value, String>;
    /// The sources in the scene on air (`ferrocast_studio::SourceInfo[]`), bottom first.
    async fn sources(&self) -> Result<Value, String>;
    /// Adds a source; `fit` fits it to the screen. Answers its entry (as listed) and settings.
    async fn add_source(&self, add: AddSource) -> Result<Value, String>;
    async fn source_settings(&self, name: String) -> Result<Value, String>;
    /// A source's options as they look for its settings now (`OptionInfo[]`).
    async fn source_options(&self, name: String) -> Result<Value, String>;
    /// Changes only the keys given.
    async fn update_source(&self, name: String, settings: Value) -> Result<(), String>;
    async fn edit_source(&self, name: String, edit: SourceEdit) -> Result<(), String>;
    /// Streams the program over RTMP (Ferrocast's own Go live ingest) → stats.
    async fn stream_start(&self, server: String, key: String) -> Result<Value, String>;
    async fn stream_stop(&self) -> Result<(), String>;
    /// `ferrocast_studio::StreamStats` plus `error`, or null when not streaming.
    async fn stream_stats(&self) -> Result<Value, String>;
}

/// `POST /studio/engine/sources`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct AddSource {
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub settings: Value,
    #[serde(default)]
    pub fit: bool,
}

/// What can be done to a source besides its settings (`POST …/edit` `{ "do": "fit" }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", tag = "do")]
pub enum SourceEdit {
    Show,
    Hide,
    Fit,
    Remove,
}

/// A source or filter kind id from OBS (`color_source`, `com.example.thing`).
fn check_kind(kind: &str) -> Result<(), EngineError> {
    if kind.is_empty() || kind.len() > 128 || !kind.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b)) {
        return Err(EngineError::Invalid("kind: an OBS source or filter id, like color_source.".into()));
    }
    Ok(())
}

/// A source name: 1 to 100 letters on one line.
fn check_source_name(name: &str) -> Result<(), EngineError> {
    let n = name.chars().count();
    if n == 0 || n > 100 || name.trim() != name || name.chars().any(char::is_control) {
        return Err(EngineError::Invalid("name: 1 to 100 letters on one line.".into()));
    }
    Ok(())
}

fn check_settings(v: &Value) -> Result<(), EngineError> {
    if !(v.is_object() || v.is_null()) {
        return Err(EngineError::Invalid("settings: an object of OBS setting keys and values.".into()));
    }
    if v.to_string().len() > 256 * 1024 {
        return Err(EngineError::Invalid("settings: up to 256 KB.".into()));
    }
    Ok(())
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
        check_kind(kind)?;
        self.studio_host()?.options(kind.to_string()).await.map_err(failed)
    }

    pub async fn studio_defaults(&self, kind: &str) -> Result<Value, EngineError> {
        check_kind(kind)?;
        self.studio_host()?.defaults(kind.to_string()).await.map_err(failed)
    }

    pub async fn studio_sources(&self) -> Result<Value, EngineError> {
        self.studio_host()?.sources().await.map_err(failed)
    }

    pub async fn studio_add_source(&self, mut add: AddSource) -> Result<Value, EngineError> {
        check_kind(&add.kind)?;
        check_source_name(&add.name)?;
        check_settings(&add.settings)?;
        if add.settings.is_null() {
            add.settings = serde_json::json!({});
        }
        self.studio_host()?.add_source(add).await.map_err(failed)
    }

    pub async fn studio_source_settings(&self, name: &str) -> Result<Value, EngineError> {
        check_source_name(name)?;
        self.studio_host()?.source_settings(name.to_string()).await.map_err(failed)
    }

    pub async fn studio_source_options(&self, name: &str) -> Result<Value, EngineError> {
        check_source_name(name)?;
        self.studio_host()?.source_options(name.to_string()).await.map_err(failed)
    }

    pub async fn studio_update_source(&self, name: &str, settings: Value) -> Result<(), EngineError> {
        check_source_name(name)?;
        if !settings.is_object() {
            return Err(EngineError::Invalid("settings: an object of OBS setting keys and values.".into()));
        }
        check_settings(&settings)?;
        self.studio_host()?.update_source(name.to_string(), settings).await.map_err(failed)
    }

    pub async fn studio_edit_source(&self, name: &str, edit: SourceEdit) -> Result<(), EngineError> {
        check_source_name(name)?;
        self.studio_host()?.edit_source(name.to_string(), edit).await.map_err(failed)
    }

    /// Starts or stops streaming the Studio into this PC's Go live ingest, which sends it on
    /// to every destination switched on, as a stream from OBS would.
    pub async fn studio_stream(&self, on: bool) -> Result<Value, EngineError> {
        let host = self.studio_host()?;
        if !on {
            host.stream_stop().await.map_err(failed)?;
            return Ok(Value::Null);
        }
        let (server, key) = self.golive_ingest().await?;
        host.stream_start(server, key).await.map_err(failed)
    }

    pub async fn studio_stream_stats(&self) -> Result<Value, EngineError> {
        self.studio_host()?.stream_stats().await.map_err(failed)
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
    let refused = |status, code, message: String, hint: &str| EngineError::Refused {
        status,
        code,
        message,
        hint: hint.into(),
        retry_after: None,
    };
    if message == STOPPED {
        return refused(409, "studio_stopped", message, "Press Start in Studio, Without OBS.");
    }
    // OBS's errors come as sentence fragments ("there's no source…"): start them as sentences.
    let mut message = message.strip_prefix("name: ").map(str::to_string).unwrap_or(message);
    if let Some(c) = message.chars().next().filter(char::is_ascii_lowercase) {
        message.replace_range(..1, &c.to_ascii_uppercase().to_string());
    }
    if !message.ends_with('.') {
        message.push('.');
    }
    if message.starts_with("There's no source called") {
        return refused(404, "studio_no_source", message, "Refresh the list of sources.");
    }
    if message.contains("already a source called") {
        return refused(409, "studio_name_taken", message, "Pick another name.");
    }
    refused(
        422,
        "studio_failed",
        message,
        "Check the settings and try again. If it keeps failing, make sure OBS 32 or newer is installed.",
    )
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
        async fn defaults(&self, _kind: String) -> Result<Value, String> {
            Ok(serde_json::json!({}))
        }
        async fn sources(&self) -> Result<Value, String> {
            Ok(serde_json::json!([]))
        }
        async fn add_source(&self, add: AddSource) -> Result<Value, String> {
            Ok(serde_json::json!({ "name": add.name, "kind": add.kind, "settings": add.settings }))
        }
        async fn source_settings(&self, name: String) -> Result<Value, String> {
            Ok(serde_json::json!({ "name": name }))
        }
        async fn source_options(&self, _name: String) -> Result<Value, String> {
            Ok(serde_json::json!([]))
        }
        async fn update_source(&self, _name: String, _settings: Value) -> Result<(), String> {
            Ok(())
        }
        async fn edit_source(&self, _name: String, _edit: SourceEdit) -> Result<(), String> {
            Ok(())
        }
        async fn stream_start(&self, server: String, key: String) -> Result<Value, String> {
            Ok(serde_json::json!({ "server": server, "key_len": key.len() }))
        }
        async fn stream_stop(&self) -> Result<(), String> {
            Ok(())
        }
        async fn stream_stats(&self) -> Result<Value, String> {
            Ok(Value::Null)
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

        let add = |kind: &str, name: &str, settings: Value| AddSource {
            kind: kind.into(),
            name: name.into(),
            settings,
            fit: true,
        };
        let made = e.studio_add_source(add("color_source", "Background", Value::Null)).await.unwrap();
        assert_eq!(made["settings"], serde_json::json!({}), "no settings is an empty object");
        for (bad, field) in [
            (add("../x", "A", serde_json::json!({})), "kind"),
            (add("color_source", "", serde_json::json!({})), "name"),
            (add("color_source", " lead", serde_json::json!({})), "name"),
            (add("color_source", "A", serde_json::json!([1])), "settings"),
            (add("color_source", "A", serde_json::json!({ "x": "y".repeat(300_000) })), "settings"),
        ] {
            let err = e.studio_add_source(bad).await.unwrap_err();
            assert!(matches!(&err, EngineError::Invalid(m) if m.starts_with(field)), "{err:?}");
        }
        assert!(e.studio_update_source("Background", serde_json::json!("red")).await.is_err());
        e.studio_update_source("Background", serde_json::json!({ "color": 1 })).await.unwrap();
        e.studio_edit_source("Background", SourceEdit::Fit).await.unwrap();
        assert!(e.studio_edit_source("a\nb", SourceEdit::Remove).await.is_err());

        for (msg, status) in [
            ("there's no source called \"X\" here", 404),
            ("name: there's already a source called \"X\" here.", 409),
            ("the encoder \"x\" isn't available", 422),
        ] {
            let EngineError::Refused { status: got, message, .. } = failed(msg.into()) else { panic!() };
            assert_eq!(got, status, "{msg}");
            assert!(message.ends_with('.') && !message.starts_with(char::is_lowercase), "{message}");
            assert!(!message.starts_with("Name:"), "{message}");
        }
    }
}
