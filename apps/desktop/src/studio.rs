//! The Studio's engine in the desktop app (2.1, Track B): libobs runs on the main thread (the
//! preview is a native view in this window, which AppKit allows only there). The dashboard
//! reaches it through the engine's HTTP routes; this is the engine's `StudioHost`.
//!
//! For the 2.1 preview, OBS's engine comes from the OBS installed on this computer; a later
//! version ships it inside Ferrocast.

use std::cell::RefCell;
use std::path::PathBuf;

use ferrocast_engine::studio_host::{
    FilterEdit, ItemEdit as Edit, Monitor as Hear, PreviewRect, STOPPED, StudioAct, StudioHost, StudioStatus,
};
use ferrocast_studio::{
    Canvas, Collection, ItemEdit, Monitor, Paths, Preview, Rect, StreamSettings, Streaming, Studio, TransitionInfo,
};
use serde_json::{Value, json};
use tauri::{AppHandle, Manager};

/// Everything libobs-related, on the main thread only.
///
/// libobs loads once and stays until Ferrocast quits: it can't be shut down and started again
/// in one process (the second shutdown crashed in FreeType's cleanup, 25 Oct; OBS never does
/// it either). Stop takes the scenes and preview away; Start loads them back on the same engine.
#[derive(Default)]
struct Held {
    studio: Option<Studio>,
    collection: Option<Collection>,
    preview: Option<Preview>,
    stream: Option<Streaming>,
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

/// What the Studio streams into Go live with: the Mac's hardware H.264 when there is one,
/// else x264. Go live sends it on as it comes, so this is what viewers get (Twitch takes up
/// to 6000 kbps; Kick refuses over 8000, 25 Oct).
fn stream_settings(studio: &Studio) -> StreamSettings {
    const HARDWARE: &str = "com.apple.videotoolbox.videoencoder.ave.avc";
    let hardware = studio.kinds().encoders.iter().any(|e| e.id == HARDWARE);
    StreamSettings {
        video_encoder: if hardware { HARDWARE } else { "obs_x264" }.into(),
        video_kbps: 6000,
        audio_kbps: 160,
        keyframe_s: 2,
    }
}

fn stats_of(s: &Streaming) -> Value {
    let mut v = serde_json::to_value(s.stats()).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert("error".into(), s.last_error().map_or(Value::Null, Value::String));
    }
    v
}

/// The scenes a new collection starts with, empty: the dashboard shows what to add.
const FIRST_SCENE: &str = "Main";
/// The collection's name in its file (OBS shows it when the file is imported there).
const COLLECTION: &str = "Ferrocast";

fn item_edit(e: Edit) -> ItemEdit {
    match e {
        Edit::Show => ItemEdit::Show,
        Edit::Hide => ItemEdit::Hide,
        Edit::Lock => ItemEdit::Lock,
        Edit::Unlock => ItemEdit::Unlock,
        Edit::Fit => ItemEdit::Fit,
        Edit::Remove => ItemEdit::Remove,
        Edit::Up => ItemEdit::Up,
        Edit::Down => ItemEdit::Down,
        Edit::Top => ItemEdit::Top,
        Edit::Bottom => ItemEdit::Bottom,
    }
}

macro_rules! to_json {
    ($v:expr) => {
        serde_json::to_value($v).map_err(|e| e.to_string())
    };
}

/// Does one act on the collection → what it shows (null for a change).
fn run(c: &Collection, act: StudioAct) -> Result<Value, ferrocast_studio::StudioError> {
    use StudioAct as A;
    let done = |r: Result<(), ferrocast_studio::StudioError>| r.map(|()| Value::Null);
    let v = |x: Result<Value, String>| x.map_err(ferrocast_studio::StudioError::Failed);
    match act {
        A::Scenes => v(to_json!(json!({ "scenes": c.scenes(), "current": c.current(), "transition": c.transition() }))),
        A::AddScene { name } => done(c.add_scene(&name)),
        A::DuplicateScene { scene, name } => done(c.duplicate_scene(&scene, &name)),
        A::RemoveScene { scene } => done(c.remove_scene(&scene)),
        A::MoveScene { scene, to } => done(c.move_scene(&scene, to)),
        A::SetCurrent { scene } => done(c.set_current(&scene)),
        A::SetTransition { kind, ms } => done(c.set_transition(&TransitionInfo { kind, ms })),
        A::Items { scene } => v(to_json!(c.items(&scene)?)),
        A::AddSource { scene, source, fit } => {
            let item = c.add_source(&scene, &source.kind, &source.name, &source.settings, fit)?;
            let settings = c.settings(&source.name)?;
            v(to_json!(json!({ "item": item, "settings": settings })))
        }
        A::AddExisting { scene, name } => v(to_json!(json!({ "item": c.add_existing(&scene, &name)? }))),
        A::EditItem { scene, id, edit } => done(c.edit_item(&scene, id, item_edit(edit))),
        A::Sources => v(to_json!(c.sources())),
        A::Rename { name, new_name } => done(c.rename(&name, &new_name)),
        A::Settings { name } => c.settings(&name),
        A::Options { name } => v(to_json!(c.options(&name)?)),
        A::Update { name, settings } => done(c.update(&name, &settings)),
        A::Audio { scene } => v(to_json!(c.audio(&scene)?)),
        A::SetAudio { name, volume_db, muted, monitor } => {
            if let Some(d) = volume_db {
                c.set_volume(&name, d)?;
            }
            if let Some(m) = muted {
                c.set_muted(&name, m)?;
            }
            if let Some(m) = monitor {
                let m = match m {
                    Hear::Off => Monitor::Off,
                    Hear::OnlyMe => Monitor::OnlyMe,
                    Hear::MeAndStream => Monitor::MeAndStream,
                };
                c.set_monitor(&name, m)?;
            }
            Ok(Value::Null)
        }
        A::SetChannel { channel, device } => {
            done(c.set_channel(channel, device.as_ref().map(|d| (d.kind.as_str(), d.name.as_str(), &d.settings))))
        }
        A::Levels { names } => v(to_json!(c.levels(&names))),
        A::Filters { name } => v(to_json!(c.filters(&name)?)),
        A::AddFilter { name, filter } => done(c.add_filter(&name, &filter.kind, &filter.name, &filter.settings)),
        A::RemoveFilter { name, filter } => done(c.remove_filter(&name, &filter)),
        A::EditFilter { name, filter, edit } => done(match edit {
            FilterEdit::Enable => c.set_filter_enabled(&name, &filter, true),
            FilterEdit::Disable => c.set_filter_enabled(&name, &filter, false),
            FilterEdit::Up => c.move_filter(&name, &filter, ItemEdit::Up),
            FilterEdit::Down => c.move_filter(&name, &filter, ItemEdit::Down),
            FilterEdit::Top => c.move_filter(&name, &filter, ItemEdit::Top),
            FilterEdit::Bottom => c.move_filter(&name, &filter, ItemEdit::Bottom),
        }),
        A::FilterSettings { name, filter } => c.filter_settings(&name, &filter),
        A::FilterOptions { name, filter } => v(to_json!(c.filter_options(&name, &filter)?)),
        A::UpdateFilter { name, filter, settings } => done(c.update_filter(&name, &filter, &settings)),
    }
}

/// Writes the collection next to the plugins' config, whole or not at all (a new file, then
/// renamed over the old one).
fn save(c: &Collection, file: &std::path::Path) {
    let doc = c.save(COLLECTION);
    let tmp = file.with_extension("json.tmp");
    let written = serde_json::to_vec_pretty(&doc)
        .map_err(std::io::Error::other)
        .and_then(|bytes| std::fs::write(&tmp, bytes))
        .and_then(|()| std::fs::rename(&tmp, file));
    if let Err(e) = written {
        tracing::warn!(error = %e, "the Studio's scenes couldn't be saved");
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

    fn scenes_file(&self) -> PathBuf {
        self.data.join("studio").join("scenes.json")
    }

    fn status_of(h: &Held) -> StudioStatus {
        let found = obs_paths().is_some();
        StudioStatus {
            available: found,
            running: h.collection.is_some(),
            version: h.collection.as_ref().and(h.studio.as_ref()).map(Studio::version),
            renderer: h.collection.as_ref().map(|_| h.renderer.to_string()),
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
        let file = self.scenes_file();
        self.on_main(move |h| {
            if h.collection.is_some() {
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
            // The saved scenes, or one empty scene the first time. A file that doesn't read is
            // kept aside (never overwritten) and the Studio starts fresh.
            let saved = std::fs::read(&file).ok().map(|b| serde_json::from_slice::<Value>(&b));
            let collection = match saved {
                Some(Ok(doc)) => Collection::load(studio, &doc, FIRST_SCENE),
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "the Studio's scenes didn't read; starting with a new set");
                    let _ = std::fs::rename(&file, file.with_extension("json.unreadable"));
                    Collection::new(studio, FIRST_SCENE)
                }
                None => Collection::new(studio, FIRST_SCENE),
            }
            .map_err(|e| e.to_string())?;
            save(&collection, &file);
            h.collection = Some(collection);
            Ok(Self::status_of(h))
        })
        .await?
    }

    async fn stop(&self) -> Result<StudioStatus, String> {
        let file = self.scenes_file();
        self.on_main(move |h| {
            // The stream, the preview and the scenes go (saved first); the engine stays loaded
            // for the next Start.
            h.stream = None;
            h.preview = None;
            if let Some(c) = h.collection.take() {
                save(&c, &file);
            }
            Self::status_of(h)
        })
        .await
    }

    async fn kinds(&self) -> Result<Value, String> {
        self.on_main(|h| {
            let s = h.collection.as_ref().and(h.studio.as_ref()).ok_or(STOPPED)?;
            serde_json::to_value(s.kinds()).map_err(|e| e.to_string())
        })
        .await?
    }

    async fn options(&self, kind: String) -> Result<Value, String> {
        self.on_main(move |h| {
            let s = h.collection.as_ref().and(h.studio.as_ref()).ok_or(STOPPED)?;
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
            let studio = h.collection.as_ref().and(h.studio.as_ref()).ok_or(STOPPED)?;
            let handle = raw_window_handle::HasWindowHandle::window_handle(&window).map_err(|e| e.to_string())?;
            h.preview = Some(studio.preview(handle.as_raw(), rect, scale).map_err(|e| e.to_string())?);
            Ok(())
        })
        .await?
    }

    async fn defaults(&self, kind: String) -> Result<Value, String> {
        self.on_main(move |h| {
            let s = h.collection.as_ref().and(h.studio.as_ref()).ok_or(STOPPED)?;
            s.defaults(&kind).map_err(|e| e.to_string())
        })
        .await?
    }

    async fn act(&self, act: StudioAct) -> Result<Value, String> {
        let file = self.scenes_file();
        self.on_main(move |h| {
            let c = h.collection.as_ref().ok_or(STOPPED)?;
            let changes = act.changes();
            let out = run(c, act).map_err(|e| e.to_string());
            if changes && out.is_ok() {
                save(c, &file);
            }
            out
        })
        .await?
    }

    async fn stream_start(&self, server: String, key: String) -> Result<Value, String> {
        self.on_main(move |h| {
            let studio = h.collection.as_ref().and(h.studio.as_ref()).ok_or(STOPPED)?;
            if let Some(s) = h.stream.as_ref().filter(|s| s.stats().active) {
                return Ok(stats_of(s));
            }
            // A stopped or failed one goes first: one output at a time.
            h.stream = None;
            let settings = stream_settings(studio);
            tracing::info!(encoder = %settings.video_encoder, kbps = settings.video_kbps, "the Studio starts streaming into Go live");
            let stream = studio.stream(&server, &key, &settings).map_err(|e| e.to_string())?;
            let v = stats_of(&stream);
            h.stream = Some(stream);
            Ok(v)
        })
        .await?
    }

    async fn stream_stop(&self) -> Result<(), String> {
        self.on_main(|h| {
            h.stream = None;
        })
        .await
    }

    async fn stream_stats(&self) -> Result<Value, String> {
        self.on_main(|h| h.stream.as_ref().map_or(Value::Null, stats_of)).await
    }
}
