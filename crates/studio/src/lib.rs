//! The Studio's engine (2.1, LIVE-STUDIO.md Track B): OBS's own libobs and plugins, loaded at
//! runtime, behind a small safe API. Every source, filter, transition and encoder OBS has is
//! here unchanged (STUDIO-PARITY.md); options are read live through `obs_properties`, so a
//! generic settings form shows every one of them.
//!
//! libobs is one global engine per process: [`Studio::start`] refuses a second one, and
//! dropping the [`Studio`] shuts it down after every scene, source and output is released.

mod ffi;

use std::ffi::c_int;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use raw_window_handle::RawWindowHandle;
use serde::{Deserialize, Serialize};

use ffi::{Api, cstr, text};

static RUNNING: AtomicBool = AtomicBool::new(false);

/// Plugins that only work inside OBS's own (Qt) window; the Studio has its own interface
/// (spikes/libobs-mac: they crash without a QApplication).
const UI_ONLY: [&str; 6] =
    ["frontend-tools", "decklink-output-ui", "aja-output-ui", "decklink-captions", "obs-websocket", "aja"];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StudioError {
    #[error("OBS's engine couldn't be loaded: {0}")]
    Load(String),
    #[error("the Studio is already running")]
    AlreadyRunning,
    #[error("OBS's engine didn't start")]
    Startup,
    #[error("the picture couldn't be set up (code {0})")]
    Video(i32),
    #[error("sound couldn't be set up")]
    Audio,
    #[error("{0}")]
    Failed(String),
}

/// Where libobs, its renderer, its data and its plugins are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// The libobs library file itself.
    pub libobs: PathBuf,
    /// The renderer (libobs-metal / libobs-opengl / libobs-d3d11), with its file extension.
    pub renderer: PathBuf,
    /// libobs's own data (shaders): ends with a separator.
    pub data: String,
    /// Plugin binaries, `%module%` standing for each plugin's name.
    pub plugin_bin: String,
    /// Plugin data, `%module%` likewise.
    pub plugin_data: String,
    /// Libraries libobs finds by `@rpath`, loaded first (an OBS.app's Frameworks). None when
    /// they sit where libobs looks (inside Ferrocast.app).
    pub deps: Option<PathBuf>,
}

impl Paths {
    /// An installed OBS.app (macOS): `contents` is `…/OBS.app/Contents`.
    pub fn from_obs_app(contents: &Path, metal: bool) -> Paths {
        let c = contents.display();
        let fw = contents.join("Frameworks");
        Paths {
            libobs: fw.join("libobs.framework/Versions/A/libobs"),
            renderer: fw.join(if metal { "libobs-metal.dylib" } else { "libobs-opengl.dylib" }),
            data: format!("{c}/Frameworks/libobs.framework/Resources/"),
            plugin_bin: format!("{c}/PlugIns/%module%.plugin/Contents/MacOS/"),
            plugin_data: format!("{c}/PlugIns/%module%.plugin/Contents/Resources/"),
            deps: Some(fw),
        }
    }
}

/// The canvas: what's composed and sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Default for Canvas {
    fn default() -> Self {
        Canvas { width: 1920, height: 1080, fps: 30 }
    }
}

/// A kind of source, filter, transition, encoder or output, as OBS names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Kind {
    pub id: String,
    pub name: String,
}

/// Every kind the loaded plugins offer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Kinds {
    pub inputs: Vec<Kind>,
    pub filters: Vec<Kind>,
    pub transitions: Vec<Kind>,
    pub encoders: Vec<Kind>,
    pub outputs: Vec<Kind>,
}

/// One option of a source or filter, enough to draw it in a settings form.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OptionInfo {
    /// The settings key.
    pub key: String,
    pub label: String,
    /// bool, int, float, text, path, list, color, color_alpha, button, font, editable_list,
    /// frame_rate, group.
    pub kind: &'static str,
    pub help: String,
    pub visible: bool,
    pub enabled: bool,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub step: Option<f64>,
    /// A list's choices, as shown.
    pub choices: Vec<String>,
    /// A group's own options.
    pub group: Vec<OptionInfo>,
}

fn kind_name(t: c_int) -> &'static str {
    match t {
        1 => "bool",
        2 => "int",
        3 => "float",
        4 => "text",
        5 => "path",
        6 => "list",
        7 => "color",
        8 => "button",
        9 => "font",
        10 => "editable_list",
        11 => "frame_rate",
        12 => "group",
        13 => "color_alpha",
        _ => "other",
    }
}

/// A box inside a window, in points from its top-left corner (the page's own coordinates).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// OBS's engine, running.
#[derive(Debug)]
pub struct Studio {
    api: Api,
    canvas: Canvas,
}

impl Studio {
    /// Loads libobs, starts it with `canvas`, and loads every plugin but the interface-only
    /// ones. `config` holds the plugins' own settings and caches.
    pub fn start(paths: &Paths, canvas: Canvas, config: &Path) -> Result<Studio, StudioError> {
        if RUNNING.swap(true, Ordering::SeqCst) {
            return Err(StudioError::AlreadyRunning);
        }
        match Self::start_inner(paths, canvas, config) {
            Ok(s) => Ok(s),
            Err(e) => {
                RUNNING.store(false, Ordering::SeqCst);
                Err(e)
            }
        }
    }

    fn start_inner(paths: &Paths, canvas: Canvas, config: &Path) -> Result<Studio, StudioError> {
        let api = Api::load(&paths.libobs, paths.deps.as_deref()).map_err(StudioError::Load)?;
        api.base_set_log_handler(Some(ffi::log_to_tracing), ptr::null_mut());
        let _ = std::fs::create_dir_all(config);
        let locale = cstr("en-US");
        let cfg = cstr(&format!("{}/", config.display()));
        if !api.obs_startup(locale.as_ptr(), cfg.as_ptr(), ptr::null_mut()) && !api.obs_initialized() {
            return Err(StudioError::Startup);
        }
        let data = cstr(&paths.data);
        api.obs_add_data_path(data.as_ptr());

        let audio = ffi::AudioInfo { samples_per_sec: 48_000, speakers: ffi::SPEAKERS_STEREO };
        if !api.obs_reset_audio(&audio) {
            api.obs_shutdown();
            return Err(StudioError::Audio);
        }
        let renderer = cstr(&paths.renderer.display().to_string());
        let mut video = ffi::VideoInfo {
            graphics_module: renderer.as_ptr(),
            fps_num: canvas.fps,
            fps_den: 1,
            base_width: canvas.width,
            base_height: canvas.height,
            output_width: canvas.width,
            output_height: canvas.height,
            output_format: ffi::VIDEO_FORMAT_NV12,
            adapter: 0,
            gpu_conversion: true,
            colorspace: ffi::VIDEO_CS_709,
            range: ffi::VIDEO_RANGE_PARTIAL,
            scale_type: ffi::OBS_SCALE_BICUBIC,
        };
        let code = api.obs_reset_video(&mut video);
        if code != ffi::OBS_VIDEO_SUCCESS {
            api.obs_shutdown();
            return Err(StudioError::Video(code));
        }
        let (bin, pdata) = (cstr(&paths.plugin_bin), cstr(&paths.plugin_data));
        api.obs_add_module_path(bin.as_ptr(), pdata.as_ptr());
        for m in UI_ONLY {
            let name = cstr(m);
            api.obs_add_disabled_module(name.as_ptr());
        }
        api.obs_load_all_modules();
        api.obs_post_load_modules();
        Ok(Studio { api, canvas })
    }

    pub fn canvas(&self) -> Canvas {
        self.canvas
    }

    /// libobs's version ("32.2.2").
    pub fn version(&self) -> String {
        text(self.api.obs_get_version_string())
    }

    /// Every kind the loaded plugins offer.
    pub fn kinds(&self) -> Kinds {
        let list = |f: &dyn Fn(usize, *mut *const std::ffi::c_char) -> bool, encoder: bool| {
            let mut out = Vec::new();
            for i in 0.. {
                let mut id = ptr::null();
                if !f(i, &mut id) {
                    break;
                }
                let name = if encoder {
                    text(self.api.obs_encoder_get_display_name(id))
                } else {
                    text(self.api.obs_source_get_display_name(id))
                };
                out.push(Kind { id: text(id), name });
            }
            out
        };
        Kinds {
            inputs: list(&|i, p| self.api.obs_enum_input_types(i, p), false),
            filters: list(&|i, p| self.api.obs_enum_filter_types(i, p), false),
            transitions: list(&|i, p| self.api.obs_enum_transition_types(i, p), false),
            encoders: list(&|i, p| self.api.obs_enum_encoder_types(i, p), true),
            outputs: list(&|i, p| self.api.obs_enum_output_types(i, p), false)
                .into_iter()
                .map(|k| Kind { name: k.id.clone(), ..k })
                .collect(),
        }
    }

    /// Every option of a kind of source or filter, read from a private (unlisted) source the
    /// way OBS's own dialog does: some plugins can't describe themselves without one. A camera
    /// or screen kind may ask macOS for permission the first time.
    pub fn options(&self, kind: &str) -> Result<Vec<OptionInfo>, StudioError> {
        let id = cstr(kind);
        let name = cstr("ferrocast-probe");
        let src = self.api.obs_source_create_private(id.as_ptr(), name.as_ptr(), ptr::null_mut());
        if src.is_null() {
            return Err(StudioError::Failed(format!("OBS has no source kind \"{kind}\".")));
        }
        let props = self.api.obs_source_properties(src);
        let out = if props.is_null() { Vec::new() } else { self.read_options(props) };
        if !props.is_null() {
            self.api.obs_properties_destroy(props);
        }
        self.api.obs_source_release(src);
        Ok(out)
    }

    fn read_options(&self, props: *mut ffi::ObsProperties) -> Vec<OptionInfo> {
        let a = &self.api;
        let mut out = Vec::new();
        let mut p = a.obs_properties_first(props);
        while !p.is_null() {
            let t = a.obs_property_get_type(p);
            let kind = kind_name(t);
            let (min, max, step) = match kind {
                "int" => (
                    Some(f64::from(a.obs_property_int_min(p))),
                    Some(f64::from(a.obs_property_int_max(p))),
                    Some(f64::from(a.obs_property_int_step(p))),
                ),
                "float" => (
                    Some(a.obs_property_float_min(p)),
                    Some(a.obs_property_float_max(p)),
                    Some(a.obs_property_float_step(p)),
                ),
                _ => (None, None, None),
            };
            let choices = if kind == "list" {
                (0..a.obs_property_list_item_count(p)).map(|i| text(a.obs_property_list_item_name(p, i))).collect()
            } else {
                Vec::new()
            };
            let group = if kind == "group" {
                let g = a.obs_property_group_content(p);
                if g.is_null() { Vec::new() } else { self.read_options(g) }
            } else {
                Vec::new()
            };
            out.push(OptionInfo {
                key: text(a.obs_property_name(p)),
                label: text(a.obs_property_description(p)),
                kind,
                help: text(a.obs_property_long_description(p)),
                visible: a.obs_property_visible(p),
                enabled: a.obs_property_enabled(p),
                min,
                max,
                step,
                choices,
                group,
            });
            if !a.obs_property_next(&mut p) {
                break;
            }
        }
        out
    }

    /// A new, empty scene.
    pub fn scene(&self, name: &str) -> Result<Scene<'_>, StudioError> {
        let n = cstr(name);
        let s = self.api.obs_scene_create(n.as_ptr());
        if s.is_null() {
            return Err(StudioError::Failed(format!("the scene \"{name}\" couldn't be made")));
        }
        Ok(Scene { studio: self, ptr: s, sources: std::cell::RefCell::new(Vec::new()) })
    }

    /// Puts a scene on air (what's recorded and streamed).
    pub fn set_program(&self, scene: &Scene<'_>) {
        self.api.obs_set_output_source(0, self.api.obs_scene_get_source(scene.ptr));
    }

    /// Frames drawn since start, and how many were late.
    pub fn frames(&self) -> (u32, u32) {
        (self.api.obs_get_total_frames(), self.api.obs_get_lagged_frames())
    }

    /// Starts recording what's on air to `path` (MP4 with OBS's own writer: no helper program
    /// needed), with `video_encoder` (`obs_x264`, `com.apple.videotoolbox.videoencoder.ave.avc`…)
    /// and AAC sound.
    pub fn record(&self, path: &Path, video_encoder: &str) -> Result<Recording<'_>, StudioError> {
        let a = &self.api;
        let (vid, vname) = (cstr(video_encoder), cstr("ferrocast-video"));
        let venc = a.obs_video_encoder_create(vid.as_ptr(), vname.as_ptr(), ptr::null_mut(), ptr::null_mut());
        if venc.is_null() {
            return Err(StudioError::Failed(format!("the encoder \"{video_encoder}\" isn't available")));
        }
        let aname = cstr("ferrocast-audio");
        let mut aenc = ptr::null_mut();
        for id in ["ffmpeg_aac", "CoreAudio_AAC"] {
            let aid = cstr(id);
            aenc = a.obs_audio_encoder_create(aid.as_ptr(), aname.as_ptr(), ptr::null_mut(), 0, ptr::null_mut());
            if !aenc.is_null() {
                break;
            }
        }
        if aenc.is_null() {
            a.obs_encoder_release(venc);
            return Err(StudioError::Failed("no AAC encoder is available".into()));
        }
        a.obs_encoder_set_video(venc, a.obs_get_video());
        a.obs_encoder_set_audio(aenc, a.obs_get_audio());
        let settings = a.obs_data_create();
        let (k, v) = (cstr("path"), cstr(&path.display().to_string()));
        a.obs_data_set_string(settings, k.as_ptr(), v.as_ptr());
        let (oid, oname) = (cstr("mp4_output"), cstr("ferrocast-record"));
        let out = a.obs_output_create(oid.as_ptr(), oname.as_ptr(), settings, ptr::null_mut());
        a.obs_data_release(settings);
        let rec = Recording { studio: self, out, venc, aenc };
        if out.is_null() {
            return Err(StudioError::Failed("the recorder couldn't be made".into()));
        }
        a.obs_output_set_video_encoder(out, venc);
        a.obs_output_set_audio_encoder(out, aenc, 0);
        if !a.obs_output_start(out) {
            return Err(StudioError::Failed(format!(
                "recording didn't start: {}",
                text(a.obs_output_get_last_error(out))
            )));
        }
        Ok(rec)
    }
}

impl Studio {
    /// Shows what's on air at `rect` inside `window` (the raw handle the window toolkit gives,
    /// e.g. Tauri's), drawn by libobs straight into a native view: no copies through the page.
    /// `scale` is the screen's pixels per point (2 on a Retina display). macOS, main thread.
    pub fn preview(&self, window: RawWindowHandle, rect: Rect, scale: f64) -> Result<Preview<'_>, StudioError> {
        #[cfg(target_os = "macos")]
        {
            let RawWindowHandle::AppKit(h) = window else {
                return Err(StudioError::Failed("that isn't a macOS window".into()));
            };
            let view = ffi::mac::child_view(h.ns_view.as_ptr(), rect.x, rect.y, rect.w, rect.h)
                .ok_or_else(|| StudioError::Failed("the preview can only be made on the main thread".into()))?;
            let (cx, cy) = px(rect, scale);
            let init = ffi::GsInitData {
                window: view.ptr(),
                cx,
                cy,
                num_backbuffers: 0,
                format: ffi::GS_BGRA,
                zsformat: ffi::GS_ZS_NONE,
                adapter: 0,
            };
            let display = self.api.obs_display_create(&init, 0x0000_00ff);
            if display.is_null() {
                view.remove();
                return Err(StudioError::Failed("the preview couldn't be made".into()));
            }
            let state = Box::new(ffi::DrawState { api: &self.api, base: (self.canvas.width, self.canvas.height) });
            let param = &*state as *const ffi::DrawState as *mut std::ffi::c_void;
            self.api.obs_display_add_draw_callback(display, ffi::draw_main, param);
            Ok(Preview { studio: self, display, state, view, scale })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (window, rect, scale);
            Err(StudioError::Failed("the preview isn't on this system yet".into()))
        }
    }
}

/// A rectangle in device pixels (at least 1x1).
fn px(rect: Rect, scale: f64) -> (u32, u32) {
    let s = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    ((rect.w * s).round().clamp(1.0, 16_384.0) as u32, (rect.h * s).round().clamp(1.0, 16_384.0) as u32)
}

/// A live preview in a window. Removed when dropped.
#[derive(Debug)]
pub struct Preview<'a> {
    studio: &'a Studio,
    display: *mut ffi::ObsDisplay,
    state: Box<ffi::DrawState>,
    #[cfg(target_os = "macos")]
    view: ffi::mac::ChildView,
    scale: f64,
}

impl Preview<'_> {
    /// Moves or resizes it (the page's box changed). Main thread.
    pub fn set_rect(&mut self, rect: Rect, scale: f64) {
        self.scale = scale;
        #[cfg(target_os = "macos")]
        self.view.set_frame(rect.x, rect.y, rect.w, rect.h);
        let (cx, cy) = px(rect, scale);
        self.studio.api.obs_display_resize(self.display, cx, cy);
    }
}

impl Drop for Preview<'_> {
    fn drop(&mut self) {
        let a = &self.studio.api;
        let param = &*self.state as *const ffi::DrawState as *mut std::ffi::c_void;
        a.obs_display_remove_draw_callback(self.display, ffi::draw_main, param);
        a.obs_display_destroy(self.display);
        #[cfg(target_os = "macos")]
        self.view.remove();
    }
}

impl Drop for Studio {
    fn drop(&mut self) {
        // Nothing on air; then sources finish being torn down on libobs's own thread before the
        // plugins unload (a text source freed after FreeType went away crashed, 25 Oct spike).
        self.api.obs_set_output_source(0, ptr::null_mut());
        // Every scene and source must be removed before this (Scene's drop does it): libobs 32's
        // canvas holds scenes, and one left alive is freed after its plugin unloaded (a text
        // source crashed in FreeType that way, 25 Oct). Then let the destroy thread finish.
        self.api.obs_wait_for_destroy_queue();
        std::thread::sleep(Duration::from_millis(100));
        self.api.obs_shutdown();
        RUNNING.store(false, Ordering::SeqCst);
    }
}

/// A scene. Released when dropped (sources in it stay while the scene does).
#[derive(Debug)]
pub struct Scene<'a> {
    studio: &'a Studio,
    ptr: *mut ffi::ObsScene,
    /// Our own reference to each source added, so they can be removed and released with it.
    sources: std::cell::RefCell<Vec<*mut ffi::ObsSource>>,
}

impl Scene<'_> {
    /// Adds a new source of `kind` named `name`, with OBS settings as JSON (the same keys OBS
    /// saves in its scene collections), at `pos` on the canvas.
    pub fn add(
        &self,
        kind: &str,
        name: &str,
        settings: &serde_json::Value,
        pos: (f32, f32),
    ) -> Result<(), StudioError> {
        let a = &self.studio.api;
        let json = cstr(&settings.to_string());
        let data = a.obs_data_create_from_json(json.as_ptr());
        let (id, n) = (cstr(kind), cstr(name));
        let src = a.obs_source_create(id.as_ptr(), n.as_ptr(), data, ptr::null_mut());
        if !data.is_null() {
            a.obs_data_release(data);
        }
        if src.is_null() {
            return Err(StudioError::Failed(format!("the source \"{name}\" ({kind}) couldn't be made")));
        }
        let item = a.obs_scene_add(self.ptr, src);
        if item.is_null() {
            a.obs_source_remove(src);
            a.obs_source_release(src);
            return Err(StudioError::Failed(format!("\"{name}\" couldn't be put in the scene")));
        }
        // Kept until the scene goes, which removes and releases it.
        self.sources.borrow_mut().push(src);
        a.obs_sceneitem_set_pos(item, &ffi::Vec2 { x: pos.0, y: pos.1 });
        Ok(())
    }
}

impl Drop for Scene<'_> {
    fn drop(&mut self) {
        // As OBS's own app clears a scene collection: remove each source and the scene (libobs 32
        // keeps scenes in its canvas until removed), then let go of our references.
        let a = &self.studio.api;
        for src in self.sources.borrow_mut().drain(..) {
            a.obs_source_remove(src);
            a.obs_source_release(src);
        }
        a.obs_source_remove(a.obs_scene_get_source(self.ptr));
        a.obs_scene_release(self.ptr);
    }
}

/// A recording. Stops (and waits for the file to be finished) when dropped.
#[derive(Debug)]
pub struct Recording<'a> {
    studio: &'a Studio,
    out: *mut ffi::ObsOutput,
    venc: *mut ffi::ObsEncoder,
    aenc: *mut ffi::ObsEncoder,
}

impl Recording<'_> {
    /// Stops and waits up to 10 s for the file to be written out.
    pub fn stop(self) {
        drop(self);
    }
}

impl Drop for Recording<'_> {
    fn drop(&mut self) {
        let a = &self.studio.api;
        if !self.out.is_null() {
            a.obs_output_stop(self.out);
            let until = Instant::now() + Duration::from_secs(10);
            while a.obs_output_active(self.out) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(50));
            }
            a.obs_output_release(self.out);
        }
        a.obs_encoder_release(self.venc);
        a.obs_encoder_release(self.aenc);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obs_app_paths() {
        let p = Paths::from_obs_app(Path::new("/Applications/OBS.app/Contents"), true);
        assert_eq!(p.libobs, Path::new("/Applications/OBS.app/Contents/Frameworks/libobs.framework/Versions/A/libobs"));
        assert!(p.renderer.ends_with("libobs-metal.dylib"));
        assert!(p.plugin_bin.contains("%module%.plugin/Contents/MacOS/"));
        assert!(p.data.ends_with('/'));
    }

    #[test]
    fn a_missing_engine_is_a_plain_error_and_never_blocks_a_retry() {
        let p = Paths::from_obs_app(Path::new("/nowhere/OBS.app/Contents"), true);
        let e = Studio::start(&p, Canvas::default(), Path::new("/tmp/fc-none")).unwrap_err();
        assert!(matches!(e, StudioError::Load(_)), "{e:?}");
        assert!(!RUNNING.load(Ordering::SeqCst), "a failed start frees the slot");
    }

    #[test]
    fn preview_sizes_are_device_pixels() {
        let r = Rect { x: 10.0, y: 20.0, w: 640.0, h: 360.0 };
        assert_eq!(px(r, 2.0), (1280, 720));
        assert_eq!(px(r, f64::NAN), (640, 360), "a bad scale is 1");
        assert_eq!(px(Rect { w: 0.0, h: -5.0, ..r }, 1.0), (1, 1), "never zero");
    }

    #[test]
    fn option_kinds_are_named() {
        assert_eq!(kind_name(1), "bool");
        assert_eq!(kind_name(6), "list");
        assert_eq!(kind_name(13), "color_alpha");
        assert_eq!(kind_name(99), "other");
    }
}
