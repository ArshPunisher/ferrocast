//! The Studio's engine in the desktop app (2.1, Track B): libobs runs on the main thread (the
//! preview is a native view in this window, which AppKit allows only there). The dashboard
//! reaches it through the engine's HTTP routes; this is the engine's `StudioHost`.
//!
//! For the 2.1 preview, OBS's engine comes from the OBS installed on this computer; a later
//! version ships it inside Ferrocast.

use std::cell::RefCell;
use std::path::PathBuf;

use ferrocast_engine::studio_host::{PreviewRect, StudioHost, StudioStatus};
use ferrocast_studio::{Canvas, Paths, Preview, Rect, Scene, Studio};
use serde_json::{Value, json};
use tauri::{AppHandle, Manager};

/// Everything libobs-related, on the main thread only.
///
/// libobs loads once and stays until Ferrocast quits: it can't be shut down and started again
/// in one process (the second shutdown crashed in FreeType's cleanup, 25 Oct; OBS never does
/// it either). Stop takes the scene and preview away; Start puts them back on the same engine.
#[derive(Default)]
struct Held {
    studio: Option<Studio>,
    scene: Option<Scene>,
    preview: Option<Preview>,
    renderer: &'static str,
}

thread_local! {
    static HELD: RefCell<Held> = RefCell::default();
}

/// Where OBS is installed (macOS), if it is.
fn obs_paths() -> Option<(Paths, &'static str)> {
    #[cfg(target_os = "macos")]
    {
        let mut places = vec![PathBuf::from("/Applications/OBS.app/Contents")];
        if let Some(home) = dirs::home_dir() {
            places.push(home.join("Applications/OBS.app/Contents"));
        }
        // OpenGL drew without late frames in the 25 Oct runs; Metal is tried again once the
        // preview lives in the app (STUDIO-PARITY, spikes/libobs-mac).
        places
            .into_iter()
            .find(|p| p.join("Frameworks/libobs.framework").exists())
            .map(|c| (Paths::from_obs_app(&c, false), "opengl"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

const NO_OBS: &str = "The Studio preview uses OBS's engine from the OBS installed on this computer. Install OBS 32 or newer (obsproject.com), then try again.";

#[derive(Debug, Clone)]
pub struct DesktopStudio {
    app: AppHandle,
    data: PathBuf,
}

impl DesktopStudio {
    pub fn new(app: AppHandle, data: PathBuf) -> DesktopStudio {
        DesktopStudio { app, data }
    }

    /// Runs `f` on the main thread with the libobs state and waits for its answer.
    async fn on_main<T: Send + 'static>(&self, f: impl FnOnce(&mut Held) -> T + Send + 'static) -> Result<T, String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.app
            .run_on_main_thread(move || {
                let out = HELD.with(|h| f(&mut h.borrow_mut()));
                let _ = tx.send(out);
            })
            .map_err(|e| format!("The window isn't ready: {e}"))?;
        rx.await.map_err(|_| "The window closed.".to_string())
    }

    fn status_of(h: &Held) -> StudioStatus {
        let found = obs_paths().is_some();
        StudioStatus {
            available: found,
            running: h.scene.is_some(),
            version: h.scene.as_ref().and(h.studio.as_ref()).map(Studio::version),
            renderer: h.scene.as_ref().map(|_| h.renderer.to_string()),
            detail: (!found).then(|| NO_OBS.into()),
        }
    }
}

#[async_trait::async_trait]
impl StudioHost for DesktopStudio {
    async fn status(&self) -> StudioStatus {
        self.on_main(|h| Self::status_of(h)).await.unwrap_or_default()
    }

    async fn start(&self) -> Result<StudioStatus, String> {
        let config = self.data.join("studio");
        self.on_main(move |h| {
            if h.scene.is_some() {
                return Ok(Self::status_of(h));
            }
            if h.studio.is_none() {
                let (paths, renderer) = obs_paths().ok_or(NO_OBS)?;
                let studio = Studio::start(&paths, Canvas::default(), &config).map_err(|e| e.to_string())?;
                tracing::info!(version = %studio.version(), renderer, "the Studio's engine loaded");
                h.renderer = renderer;
                h.studio = Some(studio);
            }
            let studio = h.studio.as_ref().ok_or("The Studio's engine isn't loaded.")?;
            // Something to see until scenes are built in the Studio (next steps).
            let scene = studio.scene("Studio").map_err(|e| e.to_string())?;
            scene
                .add(
                    "color_source",
                    "Background",
                    &json!({ "color": 0xFF_1A_12_0Eu32, "width": 1920, "height": 1080 }),
                    (0.0, 0.0),
                )
                .map_err(|e| e.to_string())?;
            scene
                .add(
                    "text_ft2_source",
                    "Title",
                    &json!({ "text": "Ferrocast Studio", "font": { "face": "Helvetica", "size": 140 } }),
                    (440.0, 450.0),
                )
                .map_err(|e| e.to_string())?;
            studio.set_program(&scene);
            h.scene = Some(scene);
            Ok(Self::status_of(h))
        })
        .await?
    }

    async fn stop(&self) -> Result<StudioStatus, String> {
        self.on_main(|h| {
            // The preview and the scene go; the engine stays loaded for the next Start.
            h.preview = None;
            if let Some(s) = h.studio.as_ref() {
                s.clear_program();
            }
            h.scene = None;
            Self::status_of(h)
        })
        .await
    }

    async fn kinds(&self) -> Result<Value, String> {
        self.on_main(|h| {
            let s = h.scene.as_ref().and(h.studio.as_ref()).ok_or(ferrocast_engine::studio_host::STOPPED)?;
            serde_json::to_value(s.kinds()).map_err(|e| e.to_string())
        })
        .await?
    }

    async fn options(&self, kind: String) -> Result<Value, String> {
        self.on_main(move |h| {
            let s = h.scene.as_ref().and(h.studio.as_ref()).ok_or(ferrocast_engine::studio_host::STOPPED)?;
            let opts = s.options(&kind).map_err(|e| e.to_string())?;
            serde_json::to_value(opts).map_err(|e| e.to_string())
        })
        .await?
    }

    async fn preview(&self, rect: Option<PreviewRect>) -> Result<(), String> {
        let app = self.app.clone();
        self.on_main(move |h| {
            let Some(r) = rect else {
                h.preview = None;
                return Ok(());
            };
            let rect = Rect { x: r.x, y: r.y, w: r.w, h: r.h };
            let window = app.get_webview_window("main").ok_or("The Ferrocast window isn't open.")?;
            let scale = window.scale_factor().unwrap_or(1.0);
            if let Some(p) = h.preview.as_mut() {
                p.set_rect(rect, scale);
                return Ok(());
            }
            let studio = h.scene.as_ref().and(h.studio.as_ref()).ok_or(ferrocast_engine::studio_host::STOPPED)?;
            let handle = raw_window_handle::HasWindowHandle::window_handle(&window).map_err(|e| e.to_string())?;
            h.preview = Some(studio.preview(handle.as_raw(), rect, scale).map_err(|e| e.to_string())?);
            Ok(())
        })
        .await?
    }
}
